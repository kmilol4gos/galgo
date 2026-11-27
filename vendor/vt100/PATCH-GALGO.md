# vt100 0.18.1 con parches de galgo

Copia de `vt100` 0.18.1 (MIT/Apache-2.0, ver LICENSE) con dos atributos que la crate descartaba:

- **Tachado** (SGR 9 / 29): `Cell::strikethrough()`, `Attrs::strikethrough()`, y su código en la salida de `contents_formatted`.
- **Parpadeo** (SGR 5 / 25): `Cell::blink()`, `Attrs::blink()`, y su código en la salida.

Los cambios están en `src/attrs.rs`, `src/term.rs`, `src/screen.rs` y `src/cell.rs`, y se marcan con `galgo`.
Se enlaza desde el `Cargo.toml` raíz con `[patch.crates-io]`. Al actualizar vt100, repetir estos cambios sobre la versión nueva.

## Reflow al cambiar el ancho

Al cambiar el número de columnas de la pantalla principal, las líneas envueltas se reacomodan en pantalla y en el
historial (`src/grid.rs`, `Grid::reflow`; el camino de siempre se mantiene para cambios solo de alto y para la
pantalla alternativa).

- **Líneas lógicas**: se unen las filas con `wrapped`. En una fila envuelta, las celdas en blanco por defecto del
  final son relleno (p. ej. un carácter ancho que no cabía en la fila anterior) y se quitan; las que están en o tras
  el cursor se conservan. Las celdas con color o atributos se conservan siempre.
- **Re-partición**: cada línea lógica se parte al ancho nuevo sin partir un carácter ancho (si no cabe, la fila se
  rellena y el carácter pasa a la siguiente). Con una sola columna, el carácter ancho sigue marcado como ancho y su
  continuación se regenera al volver a ser más ancha.
- **Cursor**: se sigue por su posición dentro de su línea lógica. Al final de una fila llena se conserva el «wrap
  pendiente» de vt100 (col == ancho).
- **Pantalla e historial**: la pantalla son las últimas `alto` filas con contenido (o con el cursor); lo anterior va
  al historial, que se recorta a su capacidad (se descartan las filas más antiguas).
- **Límite**: si el cursor está por encima de contenido que no cabe debajo de él en la pantalla, no puede mostrarse
  en su sitio: vt100 solo admite el buffer hasta el borde inferior. Entonces queda pegado al borde superior. El
  caso normal (cursor en el prompt, al final) no se ve afectado.
- **Callbacks**: el reflow no notifica las filas que pasan al historial (`on_row`); el daemon no usa esa notificación.

Pruebas: `cargo test -p vt100 reflow` (ancho→estrecho→ancho con cursor, CJK y emoji en todos los anchos, historial
lleno, historial en el tope, SGR, pantalla alternativa sin reacomodo, cambio solo de alto). Rendimiento:
`cargo test --release -p vt100 reflow_rendimiento -- --ignored --nocapture` (historial de 2000 filas, 200↔120
columnas: mediana 4,6 ms, máximo 9,2 ms por cambio de ancho).
