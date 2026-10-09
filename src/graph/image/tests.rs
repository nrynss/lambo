use super::*;
use crate::graph::canonical::canonical_key;

#[test]
fn image_ids_are_lowercase_alphanumeric_and_bounded() {
    for ok in ["a", "r17", "0", &"z".repeat(64)] {
        validate_image_id(ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
    }
    for bad in [
        "",
        "R17",
        "r-17",
        "r_17",
        "r 17",
        "r17]",
        "é",
        &"z".repeat(65),
    ] {
        let err = validate_image_id(bad).unwrap_err();
        if !bad.is_empty() {
            assert!(!err.contains(bad), "never quotes the id: {err}");
        }
    }
}

/// Design R8: the suffix must reach the canonical key whole, so the id alone
/// separates two images with one caption, and no text phrase or id spelling
/// can collide with it.
#[test]
fn the_suffix_survives_canonicalization_whole() {
    let key = |content: &str| canonical_key(content, |_| None);
    // Ids a stemmer or the stopword list would otherwise rewrite.
    for id in ["cats", "running", "the", "a", "ies", "abed", "r17", "0"] {
        let k = key(&image_content("outfit for Onam", id));
        assert!(
            k.split(' ').any(|t| t == format!("[image:{id}]")),
            "{id}: the suffix is one untouched token in {k:?}"
        );
    }
    assert_ne!(
        key(&image_content("outfit", "cats")),
        key(&image_content("outfit", "cat"))
    );
    assert_ne!(
        key(&image_content("outfit", "abc")),
        key("outfit image abc"),
        "a text phrase spelling the id is a different concept"
    );
    assert_eq!(
        key(&image_content("Outfit for Onam", "r17")),
        key(&image_content("outfit  for onam ", "r17")),
        "the caption canonicalizes as any text does"
    );
}

#[test]
fn image_content_appends_one_suffix_after_the_trimmed_caption() {
    assert_eq!(
        image_content("  render 17 ", "r17"),
        "render 17 [image:r17]"
    );
    check_caption("render 17").unwrap();
    assert!(check_caption("render [image:x] 17").is_err());
}

/// Review M1 (PR 4): a caption allowed because it holds no suffix token
/// still yields exactly one, the appended one, so its id alone decides the
/// key.
#[test]
fn an_allowed_caption_that_mentions_the_suffix_keeps_one_suffix_token() {
    let key = |content: &str| canonical_key(content, |_| None);
    for caption in [
        "see [image: diagram]",
        "the [image:<id>] suffix",
        "red [image:",
    ] {
        check_caption(caption).unwrap();
        let k = key(&image_content(caption, "abc"));
        let suffixes: Vec<_> = k.split(' ').filter(|t| is_image_suffix_token(t)).collect();
        assert_eq!(suffixes, ["[image:abc]"], "{caption:?}: {k:?}");
        assert_ne!(k, key(&image_content(caption, "zzz")));
    }
}

/// Review M2: the caption check runs on the canonical tokens, so a suffix
/// spelled in another case or split by an invisible character is refused
/// as surely as the plain one.
#[test]
fn a_caption_suffix_in_any_case_or_with_invisible_characters_is_refused() {
    for caption in [
        "red [IMAGE:zzz]",
        "red [Image:zzz]",
        "red [ima\u{200B}ge:zzz]",
        "red [image\u{2060}:zzz]",
        "\u{FEFF}[IMAGE:zzz] red",
        // Review M1 (PR 4): Porter's `s` rule turns these into `[image:zzz]`.
        "red [image:zzz]s",
        "red [IMAGE:ZZZ]S",
    ] {
        assert!(check_caption(caption).is_err(), "{caption:?}");
    }
    // Review M1 (PR 4): only a token that canonicalizes to `[image:<id>]`
    // with a valid id can collide; a caption that only mentions the suffix
    // is allowed.
    for caption in [
        "red image zzz",
        "image: red",
        "[img:zzz] red",
        "red silk saree",
        "red [image:",
        "see [image: diagram]",
        "the [image:<id>] suffix",
        "[image:Zz-z] red",
    ] {
        check_caption(caption).unwrap_or_else(|e| panic!("{caption:?}: {e}"));
    }
}

/// Review M2, the collision itself: two different images whose captions
/// smuggle each other's id share one canonical key, so the caption check is
/// what keeps them two concepts.
#[test]
fn two_images_that_smuggle_each_others_id_would_share_a_key_and_are_refused() {
    let key = |content: &str| canonical_key(content, |_| None);
    for (a, b) in [
        ("red [IMAGE:zzz]", "red [IMAGE:abc]"),
        ("red [ima\u{200B}ge:zzz]", "red [ima\u{200B}ge:abc]"),
        ("red [image:zzz]s", "red [image:abc]s"),
    ] {
        assert_eq!(
            key(&image_content(a, "abc")),
            key(&image_content(b, "zzz")),
            "without the check {a:?}/abc and {b:?}/zzz are one concept"
        );
        assert!(check_caption(a).is_err() && check_caption(b).is_err());
    }
}

#[test]
fn an_observation_cannot_be_an_image_concept() {
    assert!(check_image_concept_type(ConceptType::Observation).is_err());
    for t in [
        ConceptType::Entity,
        ConceptType::Logic,
        ConceptType::Constraint,
        ConceptType::Resource,
    ] {
        check_image_concept_type(t).unwrap();
    }
}

#[test]
fn default_ids_are_sixteen_hex_of_a_digest() {
    let digest: [u8; 32] = std::array::from_fn(|i| i as u8 * 7);
    assert_eq!(digest_id(&digest), "00070e151c232a31");
    let v = normalize(&[3.0, 4.0]);
    assert_eq!(v, vec![0.6, 0.8]);
    let id = vector_id(&v);
    assert_eq!(id.len(), DEFAULT_IMAGE_ID_HEX);
    validate_image_id(&id).unwrap();
    assert_eq!(id, vector_id(&normalize(&[6.0, 8.0])), "scale-free");
    assert_ne!(id, vector_id(&normalize(&[4.0, 3.0])));
    let unit = vec![0.6_f32, 0.8];
    assert_eq!(normalize(&unit), unit, "a unit vector passes bit for bit");
}

#[test]
fn the_payload_debug_never_prints_the_vector() {
    let payload = ImagePayload::Vector {
        values: vec![0.123_456; 3],
        declared: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 3,
        },
    };
    let shown = format!("{payload:?}");
    assert!(!shown.contains("0.123"), "{shown}");
    assert!(shown.contains("len: 3"), "{shown}");
}
