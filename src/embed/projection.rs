//! 3D projection of embedding vectors (PCA fitted by power iteration) and the
//! camera math for drawing them. Pure math, no terminal.

const POWER_ITERATIONS: usize = 20;
const UNFITTED_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const FIT_SEED: u64 = 0xD1B5_4A32_D192_ED03;

/// Maps vectors to 3 coordinates, roughly inside the unit ball.
#[derive(Debug, Clone)]
pub struct Projector {
    mean: Vec<f32>,
    axes: [Vec<f32>; 3],
    scale: f32,
    fitted: bool,
}

impl Projector {
    /// Unfitted projector: a fixed seeded random orthonormal basis.
    pub fn new(dims: usize) -> Self {
        let mut rng = Xorshift(UNFITTED_SEED);
        let axes = orthonormal_axes(dims, &mut rng, &[]);
        Projector {
            mean: vec![0.0; dims],
            axes,
            // Unit vectors have components of size about 1/sqrt(dims); rescale to about unit size.
            scale: (dims as f32).sqrt(),
            fitted: false,
        }
    }

    pub fn is_fitted(&self) -> bool {
        self.fitted
    }

    /// PCA of up to `samples.len()` vectors. Each axis comes from 20 power iterations on the
    /// covariance restricted to the complement of the earlier axes. Axis signs are normalized so
    /// the largest-magnitude component is positive.
    pub fn fit(&mut self, samples: &[Vec<f32>]) {
        let dims = self.mean.len();
        if samples.is_empty() || samples.iter().any(|sample| sample.len() != dims) {
            return;
        }
        let count = samples.len() as f32;
        let mut mean = vec![0.0f32; dims];
        for sample in samples {
            for (m, value) in mean.iter_mut().zip(sample) {
                *m += value / count;
            }
        }
        let centered: Vec<Vec<f32>> = samples
            .iter()
            .map(|sample| sample.iter().zip(&mean).map(|(v, m)| v - m).collect())
            .collect();

        let mut rng = Xorshift(FIT_SEED);
        let mut found: Vec<Vec<f32>> = Vec::with_capacity(3);
        for _ in 0..3 {
            let mut v = random_unit(dims, &mut rng);
            orthogonalize(&mut v, &found);
            for _ in 0..POWER_ITERATIONS {
                let mut w = vec![0.0f32; dims];
                for row in &centered {
                    let projection = dot(row, &v);
                    for (wj, xj) in w.iter_mut().zip(row) {
                        *wj += projection * xj;
                    }
                }
                orthogonalize(&mut w, &found);
                let length = norm(&w);
                if length < 1e-12 {
                    break;
                }
                v = w.iter().map(|x| x / length).collect();
            }
            // Deterministic sign: the largest-magnitude component is positive.
            let largest = v
                .iter()
                .enumerate()
                .max_by(|a, b| {
                    a.1.abs()
                        .partial_cmp(&b.1.abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(index, _)| index)
                .unwrap_or(0);
            if v[largest] < 0.0 {
                v.iter_mut().for_each(|x| *x = -*x);
            }
            found.push(v);
        }

        self.mean = mean;
        let mut axes = found.into_iter();
        self.axes = [
            axes.next().unwrap_or_else(|| vec![0.0; dims]),
            axes.next().unwrap_or_else(|| vec![0.0; dims]),
            axes.next().unwrap_or_else(|| vec![0.0; dims]),
        ];
        let max_radius = samples
            .iter()
            .map(|sample| norm(&self.project_raw(sample)))
            .fold(0.0f32, f32::max);
        self.scale = if max_radius > 1e-12 {
            // The largest radius maps to 0.9, so the cloud fills most of the canvas without clipping.
            0.9 / max_radius
        } else {
            1.0
        };
        self.fitted = true;
    }

    /// Projects a vector to 3 coordinates, roughly inside the unit ball.
    pub fn project(&self, vector: &[f32]) -> [f32; 3] {
        let raw = self.project_raw(vector);
        [
            raw[0] * self.scale,
            raw[1] * self.scale,
            raw[2] * self.scale,
        ]
    }

    fn project_raw(&self, vector: &[f32]) -> [f32; 3] {
        let centered: Vec<f32> = vector.iter().zip(&self.mean).map(|(v, m)| v - m).collect();
        [
            dot(&centered, &self.axes[0]),
            dot(&centered, &self.axes[1]),
            dot(&centered, &self.axes[2]),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            yaw: 0.0,
            pitch: 0.0,
            distance: 4.0,
        }
    }
}

/// Rotate by yaw then pitch, apply perspective divide. Returns (screen_x, screen_y, depth) with
/// screen coordinates about [-1, 1] for points inside the unit ball (camera distance 4 or more)
/// and depth in [0, 1] (0 near, 1 far).
pub fn project_point(point: [f32; 3], camera: &Camera) -> (f32, f32, f32) {
    let [x, y, z] = point;
    let (sin_yaw, cos_yaw) = camera.yaw.sin_cos();
    let x1 = x * cos_yaw + z * sin_yaw;
    let z1 = -x * sin_yaw + z * cos_yaw;
    let (sin_pitch, cos_pitch) = camera.pitch.sin_cos();
    let y2 = y * cos_pitch - z1 * sin_pitch;
    let z2 = y * sin_pitch + z1 * cos_pitch;

    let distance = camera.distance.max(2.0);
    let depth_along_view = (z2 + distance).max(0.5);
    let screen_x = x1 * distance / depth_along_view;
    let screen_y = y2 * distance / depth_along_view;
    let near = distance - 1.5;
    let depth = ((depth_along_view - near) / 3.0).clamp(0.0, 1.0);
    (screen_x, screen_y, depth)
}

/// Advances the camera: yaw turns at 0.35 rad/s, pitch sways with the elapsed time.
pub fn orbit(camera: &mut Camera, dt_seconds: f32, elapsed_seconds: f32) {
    camera.yaw += 0.35 * dt_seconds.max(0.0);
    camera.pitch = 0.35 * (0.2 * elapsed_seconds).sin();
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

fn orthogonalize(v: &mut [f32], basis: &[Vec<f32>]) {
    for axis in basis {
        let projection = dot(v, axis);
        for (x, a) in v.iter_mut().zip(axis) {
            *x -= projection * a;
        }
    }
}

fn random_unit(dims: usize, rng: &mut Xorshift) -> Vec<f32> {
    let raw: Vec<f32> = (0..dims).map(|_| rng.unit()).collect();
    let length = norm(&raw).max(1e-12);
    raw.iter().map(|x| x / length).collect()
}

fn orthonormal_axes(dims: usize, rng: &mut Xorshift, fixed: &[Vec<f32>]) -> [Vec<f32>; 3] {
    let mut basis: Vec<Vec<f32>> = fixed.to_vec();
    while basis.len() < 3 {
        let mut v = random_unit(dims, rng);
        orthogonalize(&mut v, &basis);
        let length = norm(&v);
        if length > 1e-6 {
            basis.push(v.iter().map(|x| x / length).collect());
        }
    }
    [basis[0].clone(), basis[1].clone(), basis[2].clone()]
}

/// Fixed-seed xorshift, so projections are reproducible without a rand dependency.
struct Xorshift(u64);

impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in [-1, 1].
    fn unit(&mut self) -> f32 {
        (self.next_u64() % 2_000_001) as f32 / 1_000_000.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_separates_two_clusters_along_the_first_axis() {
        let dims = 32;
        let mut rng = Xorshift(12345);
        let direction = random_unit(dims, &mut rng);
        let samples: Vec<Vec<f32>> = (0..400)
            .map(|i| {
                let sign = if i < 200 { 2.0 } else { -2.0 };
                (0..dims)
                    .map(|d| sign * direction[d] + 0.3 * rng.unit())
                    .collect()
            })
            .collect();

        let mut projector = Projector::new(dims);
        projector.fit(&samples);
        assert!(projector.is_fitted());

        let first: Vec<f32> = samples[..200]
            .iter()
            .map(|s| projector.project(s)[0])
            .collect();
        let second: Vec<f32> = samples[200..]
            .iter()
            .map(|s| projector.project(s)[0])
            .collect();
        let mean_a = first.iter().sum::<f32>() / first.len() as f32;
        let mean_b = second.iter().sum::<f32>() / second.len() as f32;
        let spread = |values: &[f32], mean: f32| {
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
        };
        let within = spread(&first, mean_a).max(spread(&second, mean_b));
        assert!(
            (mean_a - mean_b).abs() > 3.0 * within,
            "means {mean_a} and {mean_b}, within-cluster spread {within}"
        );
    }

    #[test]
    fn project_is_deterministic_and_bounded() {
        let dims = 64;
        let mut rng = Xorshift(99);
        let samples: Vec<Vec<f32>> = (0..50).map(|_| random_unit(dims, &mut rng)).collect();
        let mut projector = Projector::new(dims);
        projector.fit(&samples);
        let first = projector.project(&samples[3]);
        let second = projector.project(&samples[3]);
        assert_eq!(first, second);
        for sample in &samples {
            let p = projector.project(sample);
            assert!(norm(&p) <= 1.0 + 1e-4, "radius {}", norm(&p));
        }
    }

    #[test]
    fn unfitted_projector_is_deterministic() {
        let dims = 768;
        let mut rng = Xorshift(5);
        let vector = random_unit(dims, &mut rng);
        let a = Projector::new(dims).project(&vector);
        let b = Projector::new(dims).project(&vector);
        assert_eq!(a, b);
    }

    #[test]
    fn origin_maps_to_origin_and_nearer_points_have_smaller_depth() {
        let camera = Camera {
            yaw: 0.0,
            pitch: 0.0,
            distance: 4.0,
        };
        let (x, y, _) = project_point([0.0, 0.0, 0.0], &camera);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6);

        let (_, _, near_depth) = project_point([0.0, 0.0, -0.8], &camera);
        let (_, _, far_depth) = project_point([0.0, 0.0, 0.8], &camera);
        assert!(near_depth < far_depth, "{near_depth} !< {far_depth}");
        assert!((0.0..=1.0).contains(&near_depth));
        assert!((0.0..=1.0).contains(&far_depth));
    }

    #[test]
    fn orbit_advances_yaw_monotonically() {
        let mut camera = Camera::default();
        let mut previous = camera.yaw;
        for step in 1..=30 {
            orbit(&mut camera, 0.1, step as f32 * 0.1);
            assert!(camera.yaw > previous);
            previous = camera.yaw;
        }
        assert!(camera.pitch.abs() <= 0.35 + 1e-6);
    }
}
