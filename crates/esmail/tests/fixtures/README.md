# Message fixtures

Real-world messages kept as test cases for rendering conformance and
performance. The egui frontend's render test
(`tests/render_fixtures.rs`, now in the
[esmail-egui](https://github.com/va1erian/esmail-egui) repository) runs every
`*.eml` here through `render::render_message` and its webview;
`litehtml-view-d2d` (the native frontend's webview) can render the same files
with its `view` example.

| File | Why it is here |
|---|---|
| `meilleurtaux.eml` | Salesforce Marketing Cloud newsletter: 108 tables nested up to 17 deep, 200+ inline styles, 13 remote images. Layout used to be exponential in the nesting depth and never finished; also exercises image sizing/positioning. |

## Adding one

1. In esMail, open the message and click **Export...** to save the raw `.eml`.
2. Render it with the `litehtml-view-d2d` `view` example, or with esmail-egui's
   preview mode, to check the layout before committing.
3. **Redact it before committing.** These files live in a public repository.
   Real newsletters carry the recipient in several places:
   - the recipient address (`To`, `Delivered-To`, and the `Received ... for <addr>` line);
   - delivery-route and signature headers (`Received`, `X-Received`, `ARC-*`,
     `DKIM-Signature`, `Authentication-Results`, `Received-SPF`), which also
     embed the recipient and go stale once anything is edited;
   - per-recipient tokens: `List-Unsubscribe` (a JWT holding the subscriber id),
     `Return-Path`/`Reply-To` bounce tokens, `Message-ID`, `Feedback-ID`;
   - tracking tokens in the body: click links (`?qs=...`), the open-tracking
     pixel, and personalization blobs (`datasClientMTX=...`).

   Replace the address with `recipient@example.com`, drop the route/signature
   headers, and replace token values with `REDACTED`. Edit bytes, not text
   (the files are UTF-8 with CRLF and marked `-text` in `.gitattributes`).
   `fixtures_carry_no_personal_identifiers_or_tracking_tokens` fails if the
   obvious ones are left in, but it is a backstop, not a substitute for reading
   the file.

## Measuring

The egui frontend's `render_fixtures` test (in esmail-egui) prints wall-clock
time per fixture and width and, with `RUST_LOG=egui_litehtml_webview=debug`,
the parse / layout / record split for each pass. Debug builds are 5-10x slower
because the C++ layout engine is built unoptimized.
