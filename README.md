# xlibre-compositor-poc

PoC experimental de XLibre/X11 → EGL → OpenGL em Rust.

## Diagnóstico

```bash
cargo run -- --diagnostics
```

O programa consulta conexão X11, vendor e versão do servidor, Composite,
Damage, DRI3, Present, RandR, XFixes, EGL e OpenGL.

## Executar

```bash
cargo run
```

Uma janela X11 de 640x360 deve aparecer com fundo azul-escuro.
`WM_DELETE_WINDOW` e `KeyPress` encerram a PoC.

Esta milestone não implementa XComposite, redirection, Damage,
NameWindowPixmap, texturas de janelas ou composição.
