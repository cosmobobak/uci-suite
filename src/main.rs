//! An EPD test suite runner for UCI chess engines.

use anyhow::Context;
use shakmaty::{fen::Fen, san::San, Chess};
use std::{
    io::{BufRead, BufReader, Write},
    sync::atomic::AtomicUsize,
};

use clap::Parser;

const CONTROL_GREY: &str = "\u{001b}[38;5;243m";
const CONTROL_GREEN: &str = "\u{001b}[32m";
const CONTROL_RED: &str = "\u{001b}[31m";
const CONTROL_RESET: &str = "\u{001b}[0m";

#[derive(Parser)]
#[clap(author, version, about)]
#[allow(clippy::struct_excessive_bools, clippy::option_option)]
pub struct Cli {
    /// Path to a UCI chess engine to run on the test suite.
    pub engine: std::path::PathBuf,
    /// Path to an Extended Position Description file to use.
    #[clap(long, value_name = "PATH")]
    pub epdpath: Option<std::path::PathBuf>,
    /// UCI options to set before running the test suite.
    #[clap(long, value_name = "NAME=VALUE")]
    pub option: Vec<String>,
    /// Run the test suite in verbose mode.
    #[clap(short, long)]
    pub verbose: bool,
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
    let epd_text = cli.epdpath.map_or(WIN_AT_CHESS, |path| {
        std::fs::read_to_string(path)
            .expect("Failed to read EPD file")
            .leak()
    });

    // Parse the EPD file into a vector of positions.
    let positions = epd_text
        .lines()
        .map(parse_epd)
        .collect::<Result<Vec<_>, _>>()?;

    let mut engine_process = std::process::Command::new(&cli.engine)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn engine process");

    // Take the engine's stdin and stdout.
    let mut engine_stdin = engine_process.stdin.take().with_context(|| {
        format!(
            "Failed to take stdin of engine process {}",
            cli.engine.display()
        )
    })?;
    let mut engine_stdout = BufReader::new(engine_process.stdout.take().with_context(|| {
        format!(
            "Failed to take stdout of engine process {}",
            cli.engine.display()
        )
    })?);

    // send the engine the UCI protocol commands to initialize it
    engine_stdin.write_all(b"uci\n").with_context(|| {
        format!(
            "Failed to write to stdin of engine process {}",
            cli.engine.display()
        )
    })?;
    engine_stdin.write_all(b"isready\n").with_context(|| {
        format!(
            "Failed to write to stdin of engine process {}",
            cli.engine.display()
        )
    })?;
    // wait for the engine to respond
    let mut engine_response = String::new();
    loop {
        engine_stdout
            .read_line(&mut engine_response)
            .with_context(|| {
                format!(
                    "Failed to read from stdout of engine process {}",
                    cli.engine.display()
                )
            })?;
        if engine_response.contains("readyok") {
            break;
        }
        if cli.verbose {
            println!("[#] {}", engine_response.trim());
        }
        engine_response.clear();
    }

    // send the engine the UCI options to set
    for option in cli.option {
        let (name, value) = option
            .split_once('=')
            .with_context(|| format!("Invalid option: {}", option))?;
        engine_stdin
            .write_all(format!("setoption name {} value {}\n", name, value).as_bytes())
            .with_context(|| {
                format!(
                    "Failed to write to stdin of engine process {}",
                    cli.engine.display()
                )
            })?;
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
    for EpdPosition {
        fen,
        best_moves,
        id,
    } in positions
    {
        // send `ucinewgame` to the engine to reset its internal state
        engine_stdin.write_all(b"ucinewgame\n").with_context(|| {
            format!(
                "Failed to write to stdin of engine process {}",
                cli.engine.display()
            )
        })?;
        // send the position to the engine
        engine_stdin
            .write_all(format!("position fen {}\n", fen).as_bytes())
            .with_context(|| {
                format!(
                    "Failed to write to stdin of engine process {}",
                    cli.engine.display()
                )
            })?;
        // send the `go` command to the engine to make it think about the position
        engine_stdin
            .write_all(format!("go movetime {}\n", time).as_bytes())
            .with_context(|| {
                format!(
                    "Failed to write to stdin of engine process {}",
                    cli.engine.display()
                )
            })?;
        let think_start = std::time::Instant::now();
        // wait for the engine to respond with `bestmove <move>`
        let mut engine_response = String::new();
        loop {
            engine_stdout
                .read_line(&mut engine_response)
                .with_context(|| {
                    format!(
                        "Failed to read from stdout of engine process {}",
                        cli.engine.display()
                    )
                })?;
            if cli.verbose {
                println!(
                    "[{CONTROL_GREY}{id:midl$}{CONTROL_RESET}] {}",
                    engine_response.trim(),
                    midl = maxidlen
                );
            }
            if engine_response.contains("bestmove") {
                break;
            }
            engine_response.clear();
        }
        // parse the engine's best move
        let engine_best_move = engine_response
            .split_whitespace()
            .nth(1)
            .with_context(|| format!("Failed to parse engine response: {}", engine_response))?;
        let think_time = think_start.elapsed();
        // check if the engine's best move matches any of the EPD's best moves
        let passed = best_moves
            .iter()
            .any(|best_move| best_move == engine_best_move);
        // print the result
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
        let move_strings = best_moves
            .iter()
            .map(move_fmt)
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "[{CONTROL_GREY}{id:midl$}{CONTROL_RESET}] {fen:mfl$} {colour}{}{CONTROL_RESET} [{move_strings}]{failinfo}",
            if passed { "PASS" } else { "FAIL" },
            midl = maxidlen,
            mfl = maxfenlen,
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
