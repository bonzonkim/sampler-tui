# Documentation Internationalization Design

## Goal

Provide complete English and Korean versions of the sampler-tui user guide while keeping the existing branch-based GitHub Pages deployment simple and build-free.

## URL structure

- `https://bonzonkim.github.io/sampler-tui/` serves the English guide.
- `https://bonzonkim.github.io/sampler-tui/ko/` serves the Korean guide.
- Both pages use the same section IDs so links such as `#record` and `#troubleshooting` remain equivalent across languages.
- Each page includes reciprocal `hreflang` links for `en` and `ko`, plus an `x-default` link to the English root.
- Each page has its own canonical URL.

The site does not redirect based on browser language. A stable URL always returns the same language, and the user changes language through the visible `EN / KO` control.

## Page and asset structure

- `docs/index.html`: English page and default route.
- `docs/ko/index.html`: Korean page.
- `docs/styles.css`: shared presentation, including the language control.
- `docs/app.js`: shared interactions.
- `docs/.nojekyll`: unchanged GitHub Pages marker.

The Korean page references assets using `../styles.css` and `../app.js`; the English page uses `./styles.css` and `./app.js`. No root-relative asset paths are introduced, so the site continues to work under the `/sampler-tui/` project path.

## Content

The current Korean page is preserved as the Korean guide. The root page is translated fully into English, including:

- navigation and section headings;
- installation, pad, capture, resampling, pattern, sample, mixer, project, MIDI, command, and troubleshooting guidance;
- buttons, labels, callouts, FAQ copy, metadata, and accessibility text;
- search placeholder, empty-state messages, copy feedback, and theme labels.

Commands, key names, file paths, technical identifiers, and keyboard layouts remain unchanged where they are language-independent.

## Shared JavaScript behavior

`docs/app.js` detects the document language from `document.documentElement.lang`. A small English/Korean string table supplies runtime-only interface text:

- light/dark theme button labels;
- search empty-state text;
- clipboard success feedback.

Search continues to index the currently rendered page only. Language navigation uses ordinary links and requires no client-side routing or saved preference.

## Navigation and accessibility

- The language control indicates the current language with `aria-current="page"`.
- Both language links remain keyboard accessible.
- Each page uses the correct `<html lang>` value.
- Existing skip links, landmarks, focus treatment, reduced-motion behavior, and responsive menu remain intact.
- Mobile and desktop layouts show the language control without displacing the primary site actions.

## README changes

The README links to both the English and Korean guides. GitHub Pages deployment remains **Deploy from a branch → main → /docs** with no workflow or build command required.

## Validation

Validation must prove:

1. Both pages contain the same required section IDs.
2. Every internal anchor resolves within its page.
3. English and Korean language links resolve reciprocally.
4. All stylesheet and script paths are relative and point to existing files.
5. Both pages have correct `lang`, canonical, and `hreflang` metadata.
6. Shared JavaScript parses and includes runtime strings for both languages.
7. The existing flat, non-glowing instrument-panel visual treatment remains unchanged.
8. `git diff --check` passes and `.superpowers` is not staged or tracked.

## Out of scope

- Automatic browser-language redirects.
- A client-side translation framework.
- A documentation build system or GitHub Actions deployment workflow.
- Languages other than English and Korean.
