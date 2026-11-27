# Fase 1: que galgo sirva como terminal de diario

Aprobado por el usuario (2026-11-27). Orden:
1. **Acentos y ñ con teclas muertas** (teclado en español en macOS). Nunca probado; iced 0.13 no trae IME.
2. **Ratón hacia los programas**: modos 1000/1002/1003 y codificación SGR 1006; rueda en pantalla alternativa. Shift fuerza la selección propia.
3. **App de verdad**: `galgo.app` con icono, instalable en Aplicaciones, que arranque desde Finder/Dock con el entorno de un login shell.
4. **Reflow al redimensionar**: las líneas envueltas se reacomodan al cambiar el ancho.
5. **OSC 52**: copiar desde programas (vim, ssh), solo escritura del portapapeles, nunca lectura, con tope de tamaño.

Cada entrega de GUI termina con una lista corta de cosas que el usuario prueba a mano antes de darla por buena.
