# Provenance and licensing notes

This branch contains a separately implemented Rust editor prototype, not an embedded or renamed build of SimpTxt. The repository branch is its publication context; the application name remains provisional.

The interface direction is inspired by Windows 11 Notepad. No Microsoft source code, branding assets, or Notepad binaries are included. The historical design reference is [Microsoft’s December 2021 announcement](https://blogs.windows.com/windows-insider/2021/12/07/redesigned-notepad-for-windows-11-begins-rolling-out-to-windows-insiders/). The prototype does not claim affiliation, endorsement or pixel-exact reproduction.

The Rust dependency versions are recorded in Cargo.lock. Principal dependencies include eframe/egui, rfd, tempfile and optional image/pollster; their upstream licenses and bundled resources remain governed by their respective notices. System fonts are discovered on the running machine, not copied into this repository.

No new project license is declared by this prototype. Publication of source is not itself a grant of an open-source license. The owner must choose and verify applicable licensing and dependency notices before distributing a licensed release.

Generated build logs, measurements and screenshots are local outputs under artifacts/ and are excluded by .gitignore. Three selected UI captures are included separately under docs/screenshots/. The verification report summarizes historical local observations, with their limitations; it does not certify a hosted CI run or cross-platform runtime behavior.
