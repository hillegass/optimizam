//! Solve a problem file and print the result file, like the `optimizam`
//! command: `cargo run --example solve_file -- examples/sudoku.json`

use optimizam::{Options, Problem};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: solve_file PROBLEM.json")?;
    let problem = Problem::from_file(&path)?;
    let result = problem.solve(&Options::default().time_limit(Duration::from_secs(60)))?;
    eprintln!("{}", result.status());
    print!("{}", result.to_json());
    Ok(())
}
