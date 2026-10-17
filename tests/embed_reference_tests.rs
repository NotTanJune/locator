//! Reference check against DigUp's vectors (needs the real text model, so it is ignored by
//! default). Run with: cargo test -- --ignored reference_vectors

use locator::embed::model::{is_present, model_path, TEXT_MODEL, VISION_MODEL};
use locator::embed::reference::{check_images, check_text, AGREEMENT};
use locator::embed::runtime::{with_embedder, EmbedderConfig};

#[test]
#[ignore = "needs the real text model; run: cargo test -- --ignored reference_vectors"]
fn reference_vectors_text_and_images_match_digup() {
    if !is_present(&TEXT_MODEL) || !is_present(&VISION_MODEL) {
        eprintln!(
            "skipped: the text and vision models must be installed (run lctr embed fetch --vision)"
        );
        return;
    }
    let config = EmbedderConfig {
        text_model: Some(model_path(&TEXT_MODEL).expect("text model path")),
        vision_model: Some(model_path(&VISION_MODEL).expect("vision model path")),
        fake: false,
    };
    let checks = with_embedder(&config, |embedder| {
        let mut checks = check_text(embedder)?;
        let (image_checks, photo_vs_screen) = check_images(embedder)?;
        checks.extend(image_checks);
        Ok((checks, photo_vs_screen))
    })
    .expect("reference check");
    let (checks, photo_vs_screen) = checks;
    assert!(
        photo_vs_screen < 0.99,
        "photo and screenshot cosine {photo_vs_screen:.6}: the image vectors do not depend on the image"
    );
    for check in &checks {
        println!("{:<14} cosine {:.6}", check.name, check.cosine);
    }
    for check in &checks {
        assert!(
            check.cosine >= AGREEMENT,
            "{}: cosine {:.6} below {AGREEMENT}",
            check.name,
            check.cosine
        );
    }
}
