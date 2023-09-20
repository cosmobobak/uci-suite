#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
//! An EPD test suite runner for UCI chess engines.

use anyhow::Context;
use shakmaty::{fen::Fen, san::San, Chess};
use std::{
    io::{BufRead, BufReader, Write},
    sync::atomic::AtomicUsize, str::FromStr,
};

use clap::Parser;

const CONTROL_GREY: &str = "\u{001b}[38;5;243m";
const CONTROL_GREEN: &str = "\u{001b}[32m";
const CONTROL_RED: &str = "\u{001b}[31m";
const CONTROL_RESET: &str = "\u{001b}[0m";

#[derive(Debug, Copy, Clone)]
pub enum InbuiltEpd {
    WinAtChess,
    Zugzwangs,
    Tablebases,
}

impl FromStr for InbuiltEpd {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "winatchess" | "wac" => Ok(Self::WinAtChess),
            "zugzwangs" | "zugts" => Ok(Self::Zugzwangs),
            "tablebases" | "tbs" => Ok(Self::Tablebases),
            _ => Err(anyhow::anyhow!("Invalid inbuilt EPD: {}", s)),
        }
    }
}

#[derive(Parser)]
#[clap(author, version, about)]
#[allow(clippy::struct_excessive_bools, clippy::option_option)]
pub struct Cli {
    /// Path to a UCI chess engine to run on the test suite.
    pub engine: std::path::PathBuf,
    /// Selection of inbuilt EPD test suites to run.
    /// Valid values are `winatchess`, `zugzwangs`, and `tablebases`.
    #[clap(long, value_name = "NAME")]
    pub inbuilt: Option<InbuiltEpd>,
    /// Path to an Extended Position Description file to use.
    #[clap(long, value_name = "PATH")]
    pub epdpath: Option<std::path::PathBuf>,
    /// UCI options to set before running the test suite.
    #[clap(long, value_name = "NAME=VALUE")]
    pub option: Vec<String>,
    /// Run the test suite in verbose mode.
    #[clap(short, long)]
    pub verbose: bool,
    /// Run the test suite in debug mode.
    #[clap(long)]
    pub debug: bool,
    /// Time the engine should spend on each position, in milliseconds.
    #[clap(long, value_name = "MILLISECONDS")]
    pub time: Option<u64>,
}

const WIN_AT_CHESS: &str = include_str!("../epds/wac.epd");
const _ZUGZWANGS: &str = include_str!("../epds/zugts.epd");
const _TABLEBASES: &str = include_str!("../epds/tbtest.epd");

struct EpdPosition {
    fen: String,
    best_moves: Vec<String>,
    id: String,
}

fn parse_epd(line: &str) -> Result<EpdPosition, anyhow::Error> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let fen_string = line
        .split_whitespace()
        .take(4)
        .chain(Some("1 1"))
        .collect::<Vec<_>>()
        .join(" ");
    let fen: Fen = fen_string
        .parse()
        .with_context(|| format!("invalid fen: {fen_string}"))?;
    let board: Chess = fen
        .into_position(shakmaty::CastlingMode::Standard)
        .with_context(|| format!("invalid fen: {fen_string}"))?;
    let best_move_idx = line
        .find("bm")
        .with_context(|| format!("no bestmove found in {line}"))?;
    let best_moves = &line[best_move_idx + 3..];
    let end_of_best_moves = best_moves
        .find(';')
        .with_context(|| format!("no end of bestmove found in {line}"))?;
    let best_moves = &best_moves[..end_of_best_moves]
        .split(' ')
        .collect::<Vec<_>>();
    let best_moves = best_moves
        .iter()
        .map(|best_move| {
            let san: San = best_move
                .parse()
                .with_context(|| format!("invalid san: {best_move}"))?;
            let mv_string = san
                .to_move(&board)
                .with_context(|| format!("{san} is illegal in {fen_string}"))?
                .to_uci(shakmaty::CastlingMode::Standard)
                .to_string();
            Ok::<_, anyhow::Error>(mv_string)
        })
        .collect::<Result<_, _>>()?;
    let id_idx = line.find("id");
    let id = if let Some(id_idx) = id_idx {
        line[id_idx + 4..]
            .split(|c| c == '"')
            .next()
            .with_context(|| format!("no id found in {line}"))?
            .to_string()
    } else {
        format!("position {counter}")
    };
    Ok(EpdPosition {
        fen: fen_string,
        best_moves,
        id,
    })
}

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();

    // Read the EPD file into a string.
    let epd_text = cli.epdpath.as_deref().map_or(WIN_AT_CHESS, |path| {
        std::fs::read_to_string(path)
            .expect("Failed to read EPD file")
            .leak()
    });

    // Parse the EPD file into a vector of positions.
    let positions = epd_text
        .lines()
        .map(parse_epd)
        .collect::<Result<Vec<_>, _>>()?;

    let (mut engine_stdin, mut engine_stdout) = boot_engine(&cli)?;

    // send the engine the UCI protocol commands to initialize it
    write_line(cli.debug, &mut engine_stdin, "uci\n")?;
    write_line(cli.debug, &mut engine_stdin, "isready\n")?;
    // wait for the engine to respond
    loop {
        let engine_response = read_line(cli.debug, &mut engine_stdout)?;
        if engine_response.contains("readyok") {
            break;
        }
    }

    // send the engine the UCI options to set
    for option in cli.option {
        let (name, value) = option
            .split_once('=')
            .with_context(|| format!("Invalid option: {option}"))?;
        let set_option_text = format!("setoption name {name} value {value}\n");
        write_line(cli.debug, &mut engine_stdin, &set_option_text)?;
    }

    // start the testing loop -
    // for each position, send the engine the position and then check if the engine's best move matches any of
    // the best moves in the EPD entry.
    let time = cli.time.unwrap_or(1000);
    let mut successes = 0;
    let maxfenlen = positions.iter().map(|pos| pos.fen.len()).max().unwrap();
    let maxidlen = positions.iter().map(|pos| pos.id.len()).max().unwrap();
    let n = positions.len();
    let start_time = std::time::Instant::now();
    for epd in positions {
        // send `ucinewgame` to the engine to reset its internal state
        write_line(cli.debug, &mut engine_stdin, "ucinewgame\n")?;
        // send the position to the engine
        write_line(cli.debug, &mut engine_stdin, &format!("position fen {}\n", epd.fen))?;
        // send the `go` command to the engine to make it think about the position
        write_line(cli.debug, &mut engine_stdin, &format!("go movetime {time}\n"))?;
        let think_start = std::time::Instant::now();
        // wait for the engine to respond with `bestmove <move>`
        let mut engine_response;
        loop {
            engine_response = read_line(cli.debug, &mut engine_stdout)?;
            if cli.verbose {
                println!(
                    "[{CONTROL_GREY}{id:midl$}{CONTROL_RESET}] {}",
                    engine_response.trim(),
                    midl = maxidlen,
                    id = epd.id,
                );
            }
            if engine_response.contains("bestmove") {
                break;
            }
        }
        // parse the engine's best move
        let engine_best_move = engine_response
            .split_whitespace()
            .nth(1)
            .with_context(|| format!("Failed to parse engine response: {engine_response}"))?;
        let think_time = think_start.elapsed();
        // check if the engine's best move matches any of the EPD's best moves
        let passed = epd
            .best_moves
            .iter()
            .any(|best_move| best_move == engine_best_move);
        // print the result
        print_position_results(
            &epd,
            passed,
            think_time,
            engine_best_move,
            maxidlen,
            maxfenlen,
        );
        if passed {
            successes += 1;
        }
    }
    let elapsed = start_time.elapsed();
    println!(
        "{n} positions in {}.{:03}s",
        elapsed.as_secs(),
        elapsed.subsec_millis()
    );
    println!("{successes}/{n} passed");

    Ok(())
}

fn print_position_results(
    epd: &EpdPosition,
    passed: bool,
    think_time: std::time::Duration,
    engine_best_move: &str,
    maxidlen: usize,
    maxfenlen: usize,
) {
    let colour = if passed { CONTROL_GREEN } else { CONTROL_RED };
    let failinfo = if passed {
        format!(
            " {CONTROL_GREY}{:.1}s{CONTROL_RESET}",
            think_time.as_secs_f64()
        )
    } else {
        format!(" {CONTROL_GREY}{:.1}s{CONTROL_RESET} program chose {CONTROL_RED}{engine_best_move}{CONTROL_RESET}", think_time.as_secs_f64())
    };
    let move_fmt = |m: &String| {
        if m == engine_best_move {
            m.clone()
        } else {
            format!("{CONTROL_GREY}{m}{CONTROL_RESET}")
        }
    };
    let move_strings = epd
        .best_moves
        .iter()
        .map(move_fmt)
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "[{CONTROL_GREY}{id:midl$}{CONTROL_RESET}] {fen:mfl$} {colour}{}{CONTROL_RESET} [{move_strings}]{failinfo}",
        if passed { "PASS" } else { "FAIL" },
        midl = maxidlen,
        mfl = maxfenlen,
        id = epd.id,
        fen = epd.fen,
    );
}

fn boot_engine(
    cli: &Cli,
) -> Result<
    (
        std::process::ChildStdin,
        BufReader<std::process::ChildStdout>,
    ),
    anyhow::Error,
> {
    let mut engine_process = std::process::Command::new(&cli.engine)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn engine process");
    let engine_stdin = engine_process.stdin.take().with_context(|| {
        format!(
            "Failed to take stdin of engine process {}",
            cli.engine.display()
        )
    })?;
    let engine_stdout = BufReader::new(engine_process.stdout.take().with_context(|| {
        format!(
            "Failed to take stdout of engine process {}",
            cli.engine.display()
        )
    })?);
    Ok((engine_stdin, engine_stdout))
}

fn read_line(debug: bool, reader: &mut BufReader<std::process::ChildStdout>) -> Result<String, anyhow::Error> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .with_context(|| "Failed to read from engine process")?;
    if debug {
        eprintln!("[?] ENGINE -> TOOL: {}", line.trim());
    }
    Ok(line)
}

fn write_line(debug: bool, writer: &mut std::process::ChildStdin, line: &str) -> Result<(), anyhow::Error> {
    writer
        .write_all(line.as_bytes())
        .with_context(|| "Failed to write to engine process")?;
    if debug {
        eprintln!("[?] TOOL -> ENGINE: {}", line.trim());
    }
    Ok(())
}
