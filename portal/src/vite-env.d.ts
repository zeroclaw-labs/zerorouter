/// <reference types="vite/client" />

// Vite's ambient client types. This file is part of Vite's own scaffold and was
// simply missing here — nothing referenced it, so nothing complained.
//
// TypeScript 7 is what surfaced the gap: `main.tsx` does a side-effect import of
// `./styles.css`, and without a declaration for `*.css` that is a module with no
// type, which 7 rejects (TS2882) where 5.9 accepted it silently. `vite/client`
// declares `*.css` (and the CSS-module variants), so the import type-checks for
// the reason it is actually valid — Vite turns it into a stylesheet — rather
// than because the checker was not looking.
//
// It also types `import.meta.env`, which no file uses today. That is the half
// worth keeping in mind: had someone reached for `import.meta.env.MODE` before
// now, it would have been `any` rather than an error, and a typo in an env var
// name would have type-checked clean.
