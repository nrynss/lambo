//! The #22 additions to [`Embedder`]: the query role, the modalities and the
//! image default, and [`EmbedError::Unsupported`]. Every existing adapter
//! implements only `dimensions` and `embed`, so these defaults are what it
//! now answers; the tests pin them.

use super::*;

/// The smallest adapter there is: `dimensions` and `embed`, nothing else, as
/// every text adapter in the tree was written before #22. The vector encodes
/// the text's length so a test can tell which text was embedded.
struct TextOnly;

#[async_trait]
impl Embedder for TextOnly {
    fn dimensions(&self) -> usize {
        2
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable("empty".into()));
        }
        Ok(vec![text.len() as f32, 1.0])
    }
}

fn image() -> ImageInput<'static> {
    ImageInput::from_validated(b"not really a png", ImageMime::Png, [7; 32])
}

#[tokio::test]
async fn default_embed_query_is_the_document_embed() {
    let e = TextOnly;
    for text in ["a", "user schema", "  padded  "] {
        assert_eq!(
            e.embed_query(text).await.unwrap(),
            e.embed(text).await.unwrap(),
            "a symmetric adapter embeds a query exactly as a document: {text:?}"
        );
    }
}

#[tokio::test]
async fn default_embed_query_keeps_the_empty_input_refusal() {
    // CON-7 holds for the query role too, because the default is `embed`.
    let err = TextOnly.embed_query("   ").await.unwrap_err();
    assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
}

#[test]
fn default_modalities_are_text_only() {
    let m = TextOnly.modalities();
    assert_eq!(m, Modalities::TEXT);
    assert!(!m.contains(Modalities::IMAGE));
}

#[tokio::test]
async fn default_embed_image_is_unsupported_and_permanent() {
    let err = TextOnly.embed_image(image()).await.unwrap_err();
    assert!(matches!(err, EmbedError::Unsupported(_)), "{err:?}");
    assert!(
        !err.is_transient(),
        "an unsupported input never heals on retry"
    );
    assert_eq!(
        err.to_string(),
        "unsupported input: this embedder does not embed images"
    );
}

#[test]
fn unsupported_is_not_transient_and_the_old_classes_are_unchanged() {
    assert!(!EmbedError::Unsupported("x".into()).is_transient());
    assert!(EmbedError::Unavailable("x".into()).is_transient());
    assert!(!EmbedError::Backend("x".into()).is_transient());
}

#[test]
fn modalities_combine() {
    let both = Modalities::TEXT | Modalities::IMAGE;
    assert!(both.contains(Modalities::TEXT));
    assert!(both.contains(Modalities::IMAGE));
    assert_ne!(both, Modalities::TEXT);
}

#[test]
fn image_mime_parses_only_the_three_literal_types() {
    for mime in [ImageMime::Png, ImageMime::Jpeg, ImageMime::Webp] {
        assert_eq!(ImageMime::from_mime(mime.as_str()), Some(mime));
        assert_eq!(mime.to_string(), mime.as_str());
    }
    for refused in [
        "image/jpg",
        "IMAGE/PNG",
        "image/png; charset=binary",
        " image/png",
        "image/gif",
        "image/svg+xml",
        "",
    ] {
        assert_eq!(ImageMime::from_mime(refused), None, "{refused:?}");
    }
}

#[test]
fn image_input_debug_never_prints_the_bytes() {
    let secret = b"PRIVATE-OUTFIT-PIXELS";
    let input = ImageInput::from_validated(secret, ImageMime::Jpeg, [0; 32]);
    let shown = format!("{input:?}");
    assert!(!shown.contains("PRIVATE"), "{shown}");
    assert!(
        shown.contains("len: 21") && shown.contains("Jpeg"),
        "{shown}"
    );
    assert_eq!(input.bytes(), secret);
    assert_eq!(input.mime(), ImageMime::Jpeg);
}

#[cfg(feature = "embed-fixture")]
#[tokio::test]
async fn the_fixture_keeps_every_default() {
    // PR 1 changes no shipped adapter: the fixture is symmetric and text-only
    // until #22 PR 3 gives it a deterministic image embed.
    let f = FixtureEmbedder::new();
    assert_eq!(f.modalities(), Modalities::TEXT);
    assert_eq!(
        f.embed_query("red silk saree").await.unwrap(),
        f.embed("red silk saree").await.unwrap()
    );
    let err = f.embed_image(image()).await.unwrap_err();
    assert!(matches!(err, EmbedError::Unsupported(_)), "{err:?}");
}
