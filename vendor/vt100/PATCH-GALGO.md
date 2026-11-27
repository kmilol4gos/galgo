# vt100 0.18.1 con parches de galgo

Copia de `vt100` 0.18.1 (MIT/Apache-2.0, ver LICENSE) con dos atributos que la crate descartaba:

- **Tachado** (SGR 9 / 29): `Cell::strikethrough()`, `Attrs::strikethrough()`, y su código en la salida de `contents_formatted`.
- **Parpadeo** (SGR 5 / 25): `Cell::blink()`, `Attrs::blink()`, y su código en la salida.

Los cambios están en `src/attrs.rs`, `src/term.rs`, `src/screen.rs` y `src/cell.rs`, y se marcan con `galgo`.
Se enlaza desde el `Cargo.toml` raíz con `[patch.crates-io]`. Al actualizar vt100, repetir estos cambios sobre la versión nueva.
