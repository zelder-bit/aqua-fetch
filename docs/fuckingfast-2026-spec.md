# fuckingfast.co: 2026 site changes (behaviour spec)

Clean-room spec. It describes what the site does, not how any other program
does it. Implement it in `src/fuckingfast.rs` without reading JDownloader,
its mirrors, or the ffdl reference.

## What changed on the site

1. **Direct link via `/go` (since about July 2026).** A file page may have no
   `window.open("...")` or `location = "..."` direct link. Instead the page
   refers to the path `/f/<id>/go`. To get the link, the client POSTs to
   `https://fuckingfast.co/f/<id>/go`:
   - Same cookies and user agent as the page request, `Referer: <page URL>`,
     `Content-Type: application/x-www-form-urlencoded`.
   - Body: empty, or `cf-turnstile-response=<token>` when the page has a
     Turnstile widget (see 2).
   - A good reply carries the direct URL in the **`Hx-Redirect`** response
     header (htmx style). The body is typically the plain text `OK`.
     Resolve the header value against the page URL, and accept http(s) only.
   - No `Hx-Redirect` header means failure.

2. **Turnstile widget on the page (since about August 2026).** The page can
   embed a Cloudflare Turnstile captcha. It is present when the HTML contains
   `challenges.cloudflare.com/turnstile/` **and** matches `turnstileSuccess:\s*false`.
   The `/go` POST then needs a solved token. Captcha solving is a non-goal for
   Aqua Fetch, so this case maps to `Status::Blocked` with a message like
   "This file needs a captcha (Cloudflare Turnstile); open it in a browser".
   Do not POST `/go` without a token in this case.

3. **Size label.** The size can read `Size: 1.5 GB |` or `Size - 1.5 GB |`.
   Keep the existing size regex's number and unit parts exactly. Only the
   separator after `Size` widens from `:` to `:` or `-`:
   `Size\s*[:-]\s*([0-9]+(?:[.,][0-9]+)?)\s*(B|KB|MB|GB|TB|PB|KiB|MiB|GiB|TiB|PiB)`

## Order of operations in a download

1. GET the page (unchanged, including full-page Cloudflare challenge detection,
   which still maps to `Blocked` with the existing cookie message).
2. Direct link from `window.open` / `location` if present (unchanged).
3. Otherwise, if the page refers to `/f/<id>/go`:
   - Turnstile present: `Blocked` (captcha message). Stop.
   - Else: POST `/go` and take `Hx-Redirect`.
4. Otherwise: `Failed("No download link on the page (the site may have changed)")`.
5. The existing `/f/<id>/dl` POST (result ignored) stays, after the direct link
   is known and before the file GET.
6. File GET through `engine::download_from` (unchanged).

The metadata probe (`probe`) only needs name and size, so it must not POST
`/go`. A page with no direct link but with a `/go` reference counts as a good
page for probing, and so does one with a Turnstile widget. Turnstile is only
reported when downloading.

## /go errors

| Reply | Result |
|---|---|
| 2xx with `Hx-Redirect` | direct link |
| 2xx without `Hx-Redirect` | `Failed("Site gave no download link (/go)")`, not retried |
| Cloudflare challenge (existing `is_challenge`) | `Blocked` (cookie message) |
| 429, 5xx, network error | retried like the page (3 tries, backoff), then `Failed` |
| other 4xx | `Failed("HTTP <code> from /go")` |

Never log or show the `Hx-Redirect` value. It is a signed link.

## Tests (local mock server, like the existing ones)

- A page with `/f/<id>/go` and no `window.open`: POST `/go` gets an empty
  form-urlencoded body, correct Referer and cookies, and `Hx-Redirect` yields
  a relative or absolute link. The file downloads and resumes as before.
- `/go` 200 without the header gives `Failed`.
- A page with a Turnstile widget plus `/go`: `Blocked`, and no `/go` request is
  made.
- `probe` on a `/go` page returns name and size and makes no POST.
- The size regex accepts `Size - 2 GB |` and still `Size: 1.5 GB | Downloads: 12345`.
- Every existing test still passes.
