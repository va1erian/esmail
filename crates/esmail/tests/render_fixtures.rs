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

use egui_litehtml_webview::{TextRunTable, WebView, WebViewConfig, WebViewHost, WebViewSource};

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

/// What one full layout + paint of `html` at `width` points took, and the
/// content size it produced. Gives up (fails the test) after `limit`.
fn render_headless(html: String, width: f32, limit: Duration) -> (Duration, egui::Vec2) {
    let (elapsed, size, _) = render_headless_with_runs(html, width, limit);
    (elapsed, size)
}

/// As [`render_headless`], also returning where the page's text ended up.
fn render_headless_with_runs(html: String, width: f32, limit: Duration) -> (Duration, egui::Vec2, TextRunTable) {
    let ctx = egui::Context::default();
    let host = WebViewHost::new();
    let mut view: WebView = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html)));
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
    let html = esmail::render::render_message(&fixture("meilleurtaux.eml"));
    let (_, size, table) = render_headless_with_runs(html, 700.0, Duration::from_secs(120));
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

/// Timing across widths, for comparing changes. `--include-ignored` to run.
#[test]
#[ignore = "benchmark: prints timings, asserts nothing about them"]
fn bench_fixtures_across_widths() {
    for (name, raw) in fixtures() {
        let t = Instant::now();
        let html = esmail::render::render_message(&raw);
        eprintln!("{name}: render_message (parse + sanitize) {:?}", t.elapsed());
        for width in [400.0, 700.0, 1100.0] {
            let (elapsed, size) = render_headless(html.clone(), width, Duration::from_secs(600));
            eprintln!("{name} @{width}pt: {elapsed:?} (layout+paint incl. worker start), content {:.0}pt tall", size.y);
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
