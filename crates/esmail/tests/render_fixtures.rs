//! Real-world messages kept as test cases (see `tests/fixtures/README.md`):
//! each `.eml` is run through the same pipeline a live message takes --
//! `render::render_message` (MIME parse + sanitize) and then the litehtml
//! webview (layout + paint on its worker thread) -- to check both
//! *conformance* (the content that should be visible is there, nothing
//! forbidden got through) and *performance* (layout finishes in reasonable
//! time; a regression here shows up as a hang, not a slow test).
//!
//! Measure, with a per-phase breakdown from the webview's own debug logs:
//!
//! ```text
//! RUST_LOG=egui_litehtml_webview=debug \
//!   cargo test -p esmail --test render_fixtures --release -- --nocapture --include-ignored
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui_litehtml_webview::{Backend, TextRunTable, WebView, WebViewConfig, WebViewHost, WebViewSource};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures_dir().join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let mut all: Vec<_> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "eml"))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect();
    all.sort();
    all
}

/// What one full layout + paint of `html` at `width` points took under the
/// default backend, and the content size it produced. Gives up (fails the
/// test) after `limit`.
fn render_headless(html: String, width: f32, limit: Duration) -> (Duration, egui::Vec2) {
    render_headless_with(html, width, limit, Backend::default())
}

/// [`render_headless`] under a chosen backend.
fn render_headless_with(html: String, width: f32, limit: Duration, backend: Backend) -> (Duration, egui::Vec2) {
    let (elapsed, size, _) = render_headless_with_runs(html, width, limit, backend);
    (elapsed, size)
}

/// As [`render_headless_with`], also returning where the page's text ended up.
fn render_headless_with_runs(
    html: String,
    width: f32,
    limit: Duration,
    backend: Backend,
) -> (Duration, egui::Vec2, TextRunTable) {
    let ctx = egui::Context::default();
    let host = WebViewHost::new();
    let mut view: WebView =
        host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html)).with_backend(backend));
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 800.0))),
        // Left at headless egui's 2048px texture limit on purpose: this
        // message is taller than that, so it only renders because frames are
        // split into tiles that fit.
        ..Default::default()
    };
    let started = Instant::now();
    loop {
        // Nothing uploads the textures headless; egui asserts that an
        // unapplied `TexturesDelta` is cleared rather than dropped.
        ctx.run_ui(input(), |ui| {
            view.show(ui);
        }).textures_delta.clear();
        if !view.is_rendering() {
            break;
        }
        assert!(
            started.elapsed() < limit,
            "layout did not finish within {limit:?} -- exponential table-nesting layout? \
             (the litehtml dependency must include the table-cell measurement memoization)"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let elapsed = started.elapsed();
    let size = view.content_size().expect("a frame was produced");
    (elapsed, size, view.text_runs().clone())
}

// ─── meilleurtaux.eml ───────────────────────────────────────────────────────
//
// A Salesforce Marketing Cloud newsletter: 108 tables nested up to 17 deep
// (many `align=left`, `width:100%`), 200+ inline `style=` attributes, 13
// remote images, a multipart/alternative with a text part. Layout of this
// message never finished before litehtml memoized table cell measurements.

#[test]
fn meilleurtaux_renders_the_visible_content_and_nothing_forbidden() {
    let raw = fixture("meilleurtaux.eml");
    let html = esmail::render::render_message(&raw);

    // Visible copy survived MIME parsing, charset handling and sanitizing.
    for text in [
        "Rentrée 2026",
        "Les nouveautés à connaître avant de financer vos projets",
        "Je découvre les taux",
        "Auto : acheter ou louer, comment choisir ?",
        "Temps de lecture",
    ] {
        assert!(html.contains(text), "rendered HTML is missing {text:?}");
    }
    // The sanitizer's job: no script, no event handlers, no forms.
    let lower = html.to_ascii_lowercase();
    for forbidden in ["<script", "javascript:", " onclick=", " onload=", "<form", "<iframe"] {
        assert!(!lower.contains(forbidden), "sanitized HTML still contains {forbidden:?}");
    }
    // Remote images are left as-is for the webview to gate (B5), and inline
    // `style=` survives (allowlisted properties only).
    assert!(html.contains("https://image.email.meilleurtaux.com/"), "remote images must keep their URLs");
    assert!(html.contains("style=\""), "inline styles were stripped");
    // It is an HTML-only rendering of a message with no attachments.
    assert!(esmail::render::extract_attachments(&raw).is_empty());
}

#[test]
fn meilleurtaux_lays_out_in_bounded_time_at_a_plausible_height() {
    let html = esmail::render::render_message(&fixture("meilleurtaux.eml"));
    // The limit is deliberately generous (a debug build on a slow CI box):
    // this guards against the exponential blow-up, which was "never", not
    // against ordinary slowness. Use the ignored benchmark below to measure.
    let (elapsed, size) = render_headless(html, 700.0, Duration::from_secs(120));
    eprintln!("meilleurtaux.eml @700pt: {elapsed:?}, content {}x{} pt", size.x, size.y);

    assert!((size.x - 700.0).abs() < 2.0, "laid out at the wrong width: {}", size.x);
    // The newsletter is a long single column: far taller than a screen, but
    // not absurdly so (a collapsed layout is ~100pt; a runaway one is huge).
    assert!(
        (1200.0..12_000.0).contains(&size.y),
        "content height {} pt is implausible for this message",
        size.y
    );
}

#[test]
fn meilleurtaux_text_run_table_covers_the_visible_text_in_reading_order() {
    for backend in [Backend::Pixbuf, Backend::Painter] {
        text_run_table_covers_the_visible_text_in_reading_order(backend);
    }
}

fn text_run_table_covers_the_visible_text_in_reading_order(backend: Backend) {
    let html = esmail::render::render_message(&fixture("meilleurtaux.eml"));
    let (_, size, table) = render_headless_with_runs(html, 700.0, Duration::from_secs(120), backend);
    assert!(table.runs.len() > 100, "only {} runs", table.runs.len());

    // The same copy the HTML-level test looks for is present as runs, in
    // that order down the page.
    let text: String = table.runs.iter().map(|r| r.text.as_str()).collect();
    let mut last_y = f32::MIN;
    for phrase in [
        "Rentrée 2026",
        "Les nouveautés à connaître avant de financer vos projets",
        "Je découvre les taux",
        "Auto : acheter ou louer, comment choisir ?",
    ] {
        // Words and the spaces between them are separate runs, so join them
        // (as a copy would) and search the whole thing.
        assert!(text.contains(phrase), "the run table is missing {phrase:?}");
        let first_word = phrase.split(' ').next().unwrap();
        let run = table.runs.iter().find(|r| r.text == first_word).unwrap();
        assert!(run.rect.min.y >= last_y - 1.0, "{phrase:?} is out of reading order");
        last_y = run.rect.min.y;
    }

    // Every word sits inside the page and has offsets that agree with its
    // text; hidden text (the preheader) never got in. Whitespace runs are
    // exempt from the bounds: litehtml keeps collapsed whitespace boxes
    // (e.g. after the last block) that can hang a line below the page.
    for r in &table.runs {
        if r.text.trim().is_empty() {
            continue;
        }
        assert!(r.rect.min.x >= -1.0 && r.rect.max.x <= size.x + 1.0, "{:?} spills sideways: {:?}", r.text, r.rect);
        assert!(r.rect.min.y >= 0.0 && r.rect.max.y <= size.y + 1.0, "{:?} is off the page: {:?}", r.text, r.rect);
        assert_eq!(r.offsets.len(), r.text.chars().count() + 1);
        assert!(r.offsets.windows(2).all(|w| w[0] <= w[1]));
    }
}

#[test]
fn meilleurtaux_select_all_copies_readable_text() {
    // Copy is geometry over the run table, so it must read the same whichever
    // engine (and so whichever fonts) laid the page out.
    for backend in [Backend::Pixbuf, Backend::Painter] {
        select_all_copies_readable_text(backend);
    }
}

fn select_all_copies_readable_text(backend: Backend) {
    let html = esmail::render::render_message(&fixture("meilleurtaux.eml"));
    let (_, _, table) = render_headless_with_runs(html, 700.0, Duration::from_secs(120), backend);
    let copied = table.selection_text(&table.select_all().expect("the page has text"));

    // Paragraphs are separated by a blank line, in reading order.
    assert!(
        copied.contains(
            "Rentrée 2026 :

Les nouveautés à connaître avant de financer vos projets

Je découvre les taux ➔"
        ),
        "headline block did not copy as separate paragraphs:
{copied}"
    );
    // A list comes out one item per line.
    assert!(
        copied.contains(
            "Plus de transparence sur le coût réel du crédit.
Encadrement renforcé des paiements en plusieurs fois."
        ),
        "list items were not copied one per line:
{copied}"
    );
    // Text that only wrapped in the layout is one line again.
    assert!(
        copied.contains(
            "La rentrée est souvent synonyme de nouveaux projets : changement de véhicule, travaux dans le logement,"
        ),
        "a wrapped paragraph was split at the wrap:
{copied}"
    );
    // Nothing invisible or stray: the hidden preheader is absent, no run of
    // spaces, no line starting or ending in whitespace, no big vertical gaps.
    assert!(!copied.contains("  "), "double space in:
{copied}");
    assert!(!copied.contains("


"), "more than one blank line in:
{copied}");
    for line in copied.lines() {
        assert_eq!(line, line.trim(), "line with stray whitespace: {line:?}");
    }
    assert!(copied.trim() == copied);
}

/// The two backends run the same litehtml layout; only text measurement (and
/// so, at the margins, line breaks) differs -- and it can differ a lot, since
/// each picks its own fonts (Pixbuf falls back to the platform default for a
/// `font-family` list, the painter resolves it). So this is a sanity band, not
/// an equality: a collapsed layout is ~100pt and a runaway one is huge. On the
/// machine this was written on they agree to within about 1%.
#[test]
fn both_backends_lay_the_fixture_out_to_a_similar_height() {
    let html = esmail::render::render_message(&fixture("meilleurtaux.eml"));
    for width in [400.0, 700.0, 1100.0] {
        let limit = Duration::from_secs(120);
        let (_, pixbuf) = render_headless_with(html.clone(), width, limit, Backend::Pixbuf);
        let (_, painter) = render_headless_with(html.clone(), width, limit, Backend::Painter);
        eprintln!("meilleurtaux.eml @{width}pt: pixbuf {:.0}pt tall, painter {:.0}pt tall", pixbuf.y, painter.y);
        assert!((painter.x - width).abs() < 2.0, "painter laid out at the wrong width: {}", painter.x);
        let ratio = painter.y / pixbuf.y;
        assert!((0.75..1.33).contains(&ratio), "@{width}pt: painter {:.0}pt vs pixbuf {:.0}pt", painter.y, pixbuf.y);
    }
}

/// Timing across widths, for comparing changes. `--include-ignored` to run.
#[test]
#[ignore = "benchmark: prints timings, asserts nothing about them"]
fn bench_fixtures_across_widths() {
    for (name, raw) in fixtures() {
        let t = Instant::now();
        let html = esmail::render::render_message(&raw);
        eprintln!("{name}: render_message (parse + sanitize) {:?}", t.elapsed());
        for width in [400.0, 700.0, 1100.0] {
            for backend in [Backend::Pixbuf, Backend::Painter] {
                let (elapsed, size) = render_headless_with(html.clone(), width, Duration::from_secs(600), backend);
                eprintln!(
                    "{name} @{width}pt {:>7}: {elapsed:?} (layout+paint incl. worker start), content {:.0}pt tall",
                    backend.name(),
                    size.y
                );
            }
        }
    }
}

/// Utility, not a test: writes what the webview is actually given for each
/// fixture (the sanitized HTML from `render_message`) to
/// `$ESMAIL_DUMP_DIR/<fixture>.html`, so it can be fed to out-of-tree tools such
/// as `tools/render-profiler` (see docs/PERFORMANCE.md).
///
/// ```text
/// ESMAIL_DUMP_DIR=C:/prof cargo test -p esmail --test render_fixtures dump_fixtures_as_html -- --ignored
/// ```
#[test]
#[ignore = "utility: writes files to $ESMAIL_DUMP_DIR"]
fn dump_fixtures_as_html() {
    // CI runs `cargo test -- --include-ignored`, which runs this too: with no
    // directory given it must do nothing rather than fail.
    let Some(dir) = std::env::var_os("ESMAIL_DUMP_DIR").map(PathBuf::from) else {
        eprintln!("skipping: set ESMAIL_DUMP_DIR to an existing directory to write the fixtures' HTML there");
        return;
    };
    for (name, raw) in fixtures() {
        let out = dir.join(format!("{}.html", name.trim_end_matches(".eml")));
        std::fs::write(&out, esmail::render::render_message(&raw)).unwrap();
        eprintln!("wrote {}", out.display());
    }
}

// ─── every fixture ──────────────────────────────────────────────────────────

/// Fixtures are checked into a public repo: guard against a real address or
/// a recipient-linked tracking token slipping in with the next one added.
#[test]
fn fixtures_carry_no_personal_identifiers_or_tracking_tokens() {
    let all = fixtures();
    assert!(!all.is_empty());
    for (name, raw) in all {
        let text = String::from_utf8_lossy(&raw);
        let lower = text.to_ascii_lowercase();
        for header in ["delivered-to:", "\nreceived:", "x-received:", "arc-seal:", "dkim-signature:", "received-spf:"] {
            assert!(!lower.contains(header), "{name}: still has a {header:?} header (delivery route / signatures)");
        }
        // Only the reserved example domains may appear as addresses in the
        // envelope headers (the sender's own public address is fine).
        let to_line = text.lines().find(|l| l.to_ascii_lowercase().starts_with("to:")).unwrap_or("");
        assert!(to_line.contains("example.com") || to_line.contains("example.invalid"), "{name}: To is {to_line:?}");
        assert!(!lower.contains("@gmail.com"), "{name}: contains a gmail.com address");
        assert!(!lower.contains("jwt=ey"), "{name}: contains an unredacted JWT");
        // Click/open tracking tokens must be redacted.
        for param in ["qs=", "datasclientmtx=", "jwt="] {
            for (at, _) in lower.match_indices(param) {
                let value = &lower[at + param.len()..];
                assert!(value.starts_with("redacted"), "{name}: unredacted {param} token near byte {at}");
            }
        }
    }
}
