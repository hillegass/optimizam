//! Least squares: fit `y = amplitude * exp(-rate * t) + baseline` to
//! observations. `cargo run --example curve_fit`
//!
//! The same problem as `examples/curve_fit.json`. The fit is nonconvex, so
//! the global search may not prove its optimum within its node limit; the
//! result then says `locally_optimal`, with a bound.

use optimizam::{Expression, Model, Options, var};

fn main() -> Result<(), optimizam::Error> {
    let t = [0.0, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 10.0];
    let y = [10.1, 7.48, 5.62, 4.39, 3.48, 2.31, 1.79, 1.52];

    let mut m = Model::new();
    let amplitude = m.add_var(var().start(5).name("amplitude"));
    let rate = m.add_var(var().min(0).start(0.5).name("rate"));
    let baseline = m.add_var(var().start(0).name("baseline"));
    let residuals: Expression =
        t.iter().zip(y).map(|(&t, y)| (amplitude * (-t * rate).exp() + baseline - y).powi(2)).sum();
    m.minimize(residuals);

    let result = m.solve(&Options::default())?;
    let fit = result.best().expect("an unconstrained problem has a solution");
    println!("{}: sum of squares {:.6}", result.status(), fit.objective().unwrap());
    println!("  y = {:.4} * exp(-{:.4} t) + {:.4}", fit[amplitude], fit[rate], fit[baseline]);
    if let Some(bound) = result.objective_bound() {
        println!("  lower bound on the optimum: {bound:.6}");
    }
    Ok(())
}
