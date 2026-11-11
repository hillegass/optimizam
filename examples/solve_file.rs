//! Solve a problem file and print the result file, like the `optimizam`
//! command-line tool: `cargo run --example solve_file -- examples/sudoku.json`

use optimizam::{Model, Options};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: solve_file PROBLEM.json")?;
    let model = Model::from_file(&path)?;
    let result = model.solve(&Options::default().time_limit(Duration::from_secs(60)))?;
    eprintln!("{}", result.status());
    print!("{}", result.to_json());
    Ok(())
}
