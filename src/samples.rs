use serde::{Deserialize, Serialize};

use crate::pb::{Constraint, LinearProgram, ObjectiveSense, SolveRequest, Variable};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub name: String,
    pub description: String,
    pub request: SolveRequest,
    pub expected_status: i32,
    pub expected_objective: Option<f64>,
    pub expected_values: Option<Vec<f64>>,
}

pub fn fixtures() -> Vec<Sample> {
    [
        include_str!("../examples/models/production.json"),
        include_str!("../examples/models/transportation.json"),
        include_str!("../examples/models/blending.json"),
        include_str!("../examples/models/infeasible.json"),
        include_str!("../examples/models/unbounded.json"),
    ]
    .into_iter()
    .map(|s| serde_json::from_str(s).expect("valid embedded fixture"))
    .collect()
}

pub fn fixture(name: &str) -> anyhow::Result<Sample> {
    fixtures()
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("unknown model {name}"))
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Family {
    Production,
    Transportation,
}

/// Deterministic, feasible LPs. Size is products or the number of sources/destinations.
pub fn generate(family: Family, size: usize, seed: u64) -> anyhow::Result<SolveRequest> {
    anyhow::ensure!(size > 0, "size must be positive");
    let mut rng = Generator(seed);
    let mut model = LinearProgram {
        sense: ObjectiveSense::Minimize as i32,
        ..Default::default()
    };
    match family {
        Family::Transportation => {
            let count = size
                .checked_mul(size)
                .ok_or_else(|| anyhow::anyhow!("size overflow"))?;
            anyhow::ensure!(
                count <= 100_000,
                "transportation supports up to 100000 routes"
            );
            model.variables = (0..count)
                .map(|_| Variable {
                    objective: 1.0 + rng.integer(1000) as f64 / 100.0,
                    lower: Some(0.0),
                    upper: None,
                })
                .collect();
            for source in 0..size {
                model.constraints.push(Constraint {
                    indices: (0..size).map(|d| (source * size + d) as u32).collect(),
                    coefficients: vec![1.0; size],
                    lower: Some(10.0),
                    upper: Some(10.0),
                });
            }
            for destination in 0..size {
                model.constraints.push(Constraint {
                    indices: (0..size).map(|s| (s * size + destination) as u32).collect(),
                    coefficients: vec![1.0; size],
                    lower: Some(10.0),
                    upper: Some(10.0),
                });
            }
        }
        Family::Production => {
            anyhow::ensure!(size <= 100_000, "production supports up to 100000 products");
            model.sense = ObjectiveSense::Maximize as i32;
            model.variables = (0..size)
                .map(|_| Variable {
                    objective: 5.0 + rng.integer(100) as f64,
                    lower: Some(0.0),
                    upper: Some(100.0),
                })
                .collect();
            // Each product consumes up to four resources. Capacity is chosen so
            // x=10 for every product is feasible, while profits compete for capacity.
            let resources = size.div_ceil(4).max(4);
            model.constraints = vec![Constraint::default(); resources];
            for product in 0..size {
                let base = rng.integer(resources as u64) as usize;
                for offset in 0..4.min(resources) {
                    let row = &mut model.constraints[(base + offset) % resources];
                    let coefficient = 1.0 + rng.integer(10) as f64;
                    row.indices.push(product as u32);
                    row.coefficients.push(coefficient);
                    row.upper = Some(row.upper.unwrap_or(0.0) + 10.0 * coefficient);
                }
            }
        }
    }
    Ok(SolveRequest {
        request_id: format!("generated-{seed}"),
        model: Some(model),
        ..Default::default()
    })
}

// SplitMix64: reproducible across platforms and dependency upgrades.
struct Generator(u64);
impl Generator {
    fn integer(&mut self, upper: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        (z ^ (z >> 31)) % upper
    }
}
