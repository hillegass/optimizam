//! A job shop: each job runs its steps in order, each step on a given
//! machine, and a machine runs one step at a time. Minimize the time the
//! last step ends. `cargo run --example job_shop`
//!
//! The same problem as `examples/job_shop.json`.

use optimizam::{Model, Options, constraint, var};

fn main() -> Result<(), optimizam::Error> {
    // (machine, length) of each job's steps.
    let jobs = [[(0, 3), (1, 2), (2, 2)], [(1, 2), (0, 1), (2, 4)], [(1, 4), (2, 3), (0, 1)]];
    let horizon: i64 = jobs.iter().flatten().map(|s| s.1).sum();

    let mut m = Model::new();
    let makespan = m.add_var(var().integer().bounds(0, horizon).name("makespan"));
    let mut steps = Vec::new(); // (job, step, machine, start, length, interval)
    for (j, job) in jobs.iter().enumerate() {
        for (s, &(machine, length)) in job.iter().enumerate() {
            let start = m.add_var(var().integer().bounds(0, horizon).name(format!("start_{j}_{s}")));
            let interval = m.interval(start, length);
            steps.push((j, s, machine, start, length, interval));
        }
    }
    for w in steps.windows(2) {
        let ((j, _, _, start, length, _), (k, _, _, next, _, _)) = (w[0], w[1]);
        if j == k {
            m.add(constraint!(next >= start + length));
        }
    }
    for machine in 0..3 {
        let on: Vec<_> = steps.iter().filter(|s| s.2 == machine).map(|s| s.5).collect();
        m.no_overlap(&on);
    }
    m.max_of(makespan, steps.iter().map(|&(_, _, _, start, length, _)| start + length));
    m.minimize(makespan);

    let result = m.solve(&Options::default())?;
    let best = result.best().expect("the problem is feasible");
    println!("{}: makespan {}", result.status(), best[makespan]);
    for &(j, s, machine, start, length, _) in &steps {
        println!("  job {j} step {s}: machine {machine}, {} to {}", best.int(start), best.int(start) + length);
    }
    Ok(())
}
