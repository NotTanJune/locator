use locator::embed::vector::{decode_f16, dot_f16, encode_f16, truncate_and_normalize};

fn norm(values: &[f32]) -> f32 {
    values.iter().map(|v| v * v).sum::<f32>().sqrt()
}

fn sample_vector(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| ((i as f32 * 0.37).sin() * 0.9) + ((i as f32 * 0.11).cos() * 0.05))
        .collect()
}

#[test]
fn truncation_keeps_order_and_normalizes() {
    let source = sample_vector(768);
    for dims in [128usize, 256, 512, 768] {
        let truncated = truncate_and_normalize(&source, dims);
        assert_eq!(truncated.len(), dims);
        assert!(
            (norm(&truncated) - 1.0).abs() < 1e-5,
            "dims {dims} not unit norm"
        );

        // Same direction as the leading components of the source.
        let prefix_norm = norm(&source[..dims]);
        for (index, value) in truncated.iter().enumerate() {
            let expected = source[index] / prefix_norm;
            assert!((value - expected).abs() < 1e-5, "order changed at {index}");
        }
    }
}

#[test]
fn f16_round_trip_error_is_small() {
    let values: Vec<f32> = (0..=200).map(|i| -1.0 + (i as f32) * 0.01).collect();
    let decoded = decode_f16(&encode_f16(&values));
    assert_eq!(decoded.len(), values.len());
    for (original, restored) in values.iter().zip(decoded.iter()) {
        assert!(
            (original - restored).abs() < 1e-3,
            "{original} round tripped to {restored}"
        );
    }
}

#[test]
fn dot_of_normalized_vector_with_its_own_encoding_is_one() {
    let normalized = truncate_and_normalize(&sample_vector(768), 768);
    let encoded = encode_f16(&normalized);
    let score = dot_f16(&normalized, &encoded);
    assert!((score - 1.0).abs() < 1e-3, "self dot was {score}");
}

#[test]
fn zero_vector_stays_zero_without_nan() {
    let zero = vec![0.0f32; 768];
    let truncated = truncate_and_normalize(&zero, 256);
    assert_eq!(truncated.len(), 256);
    assert!(truncated.iter().all(|v| *v == 0.0));
    assert!(truncated.iter().all(|v| v.is_finite()));

    let score = dot_f16(&truncated, &encode_f16(&zero));
    assert!(score.is_finite());
    assert_eq!(score, 0.0);
}

#[test]
fn odd_byte_length_is_tolerated() {
    let values = vec![0.5f32, -0.25, 0.75];
    let mut bytes = encode_f16(&values);
    assert_eq!(bytes.len(), 6);
    bytes.push(0xAB);

    let decoded = decode_f16(&bytes);
    assert_eq!(decoded.len(), 3);
    let score = dot_f16(&values, &bytes);
    let expected: f32 = values.iter().map(|v| v * v).sum();
    assert!(
        (score - expected).abs() < 1e-3,
        "score {score} expected {expected}"
    );
}
