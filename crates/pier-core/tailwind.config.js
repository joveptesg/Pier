/**
 * Tailwind config for the Pier admin panel (pier-core).
 *
 * Tailwind v3 — matches the runtime that `cdn.tailwindcss.com` used, so all
 * existing template classes and `dark:` variants compile unchanged. Build the
 * static stylesheet with:
 *
 *   npx tailwindcss@3 -c tailwind.config.js -i assets/css/tailwind.css \
 *       -o assets/static/tailwind.css --minify
 *
 * The output `assets/static/tailwind.css` is committed and embedded via
 * rust_embed, so `cargo build` needs no Node/CLI. Re-run after adding new
 * Tailwind classes to templates. See Pier/CLAUDE.md → "Updating the panel CSS".
 */
module.exports = {
  content: ['./assets/templates/**/*.html'],
  darkMode: 'class',
  theme: {
    extend: {
      colors: {
        pier: { 50: '#eff6ff', 500: '#3b82f6', 600: '#2563eb', 700: '#1d4ed8' },
      },
    },
  },
  // No dynamic/concatenated class names exist in the templates (audited:
  // every `:class` uses object/ternary literals; the only interpolated class,
  // `levelColors[level]` in resources/detail.html, holds literal strings the
  // scanner already sees). Add tokens here if that ever changes.
  safelist: [],
};
