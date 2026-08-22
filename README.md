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

## Capturar uma janela existente

```bash
cargo run -- --capture 0x60000e
```

Este modo nomeia o backing Pixmap de uma única janela com XComposite e
importa-o como EGLImage/texture OpenGL. Ele não redireciona sub-janelas,
não possui `_NET_WM_CM_S0` e não interfere com compositores existentes.

Esta milestone não implementa redirection global, Damage, Present scheduling,
composição de todas as janelas ou integração com i3.

