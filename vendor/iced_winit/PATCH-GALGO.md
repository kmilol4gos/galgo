# iced_winit 0.13.0 con un parche de galgo

Copia de `iced_winit` 0.13.0 (MIT, ver LICENSE) con dos cambios para que el teclado de macOS escriba acentos:

1. **IME activo** (`program.rs`): tras crear cada ventana se llama a `set_ime_allowed(true)`. Sin esto winit no
   ejecuta el método de entrada y una tecla muerta (´ + e) se pierde o sale suelta (`e`, sin tilde).
2. **Texto compuesto** (`conversion.rs`): `WindowEvent::Ime(Commit(texto))` se convierte en una pulsación
   `KeyPressed` con ese texto (sin tecla física). El preedit (la tecla muerta pendiente) no produce evento.

Las pulsaciones normales siguen llegando por `KeyboardInput`: winit no envía `Commit` sin preedit, así que no hay
duplicados. Las líneas con cambios llevan el comentario `GALGO`.

Se enlaza desde `crates/galgo-gui/Cargo.toml` con `[patch.crates-io]`. Al actualizar iced, repetir estos cambios.
