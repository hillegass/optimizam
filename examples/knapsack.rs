//! A knapsack: pack the items of most value within a weight limit, and list
//! every optimal packing. `cargo run --example knapsack`
//!
//! The same problem as `examples/knapsack.json`.

use optimizam::{Expression, Model, Options, constraint, var};

fn main() -> Result<(), optimizam::Error> {
    let items = ["gold", "silver", "bronze", "gem"];
    let weight = [4, 3, 3, 2];
    let value = [10, 7, 7, 5];

    let mut m = Model::new();
    let take = m.add_vars(items.len(), var().binary().name("take"));
    let total = |w: &[i32]| -> Expression { (0..items.len()).map(|i| w[i] * take[i]).sum() };
    m.add(constraint!(total(&weight) <= 7).name("capacity"));
    m.maximize(total(&value));
    m.all_solutions(true);

    let result = m.solve(&Options::default())?;
    println!("{}: {} optimal packings", result.status(), result.solutions().len());
    for s in result.solutions() {
        let packed: Vec<&str> = (0..items.len()).filter(|&i| s.is_one(take[i])).map(|i| items[i]).collect();
        println!("  value {}: {}", s.objective().unwrap(), packed.join(", "));
    }
    Ok(())
}
