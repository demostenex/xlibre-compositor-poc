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

Nesta milestone, XDamage usa `ReportLevel::NON_EMPTY`: a extensão sinaliza que
há dano acumulado sem gerar um evento por retângulo. Depois de cada
`DamageNotify`, a PoC chama `DamageSubtract` com `None` para limpar a região
acumulada, renderiza e apresenta o frame. O event loop permanece bloqueante
quando não há eventos; não há polling nem renderização contínua.
Redimensionamento da janela fonte fica para a Milestone 2C.
