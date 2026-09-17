# Screen Streaming

Aplicativo Windows leve para transmitir a tela (ou uma janela específica) entre
computadores, com foco em baixíssimo overhead — não pode pesar o PC durante o
uso (ex: jogos rodando ao mesmo tempo).

Veja [CLAUDE_SESSIONS.md](./CLAUDE_SESSIONS.md) para o histórico de decisões,
arquitetura planejada e log de coordenação do desenvolvimento.

## Stack

- **Backend:** Rust (via [Tauri](https://tauri.app)) — captura de tela e
  codificação de vídeo com o mínimo de overhead possível.
- **UI:** HTML/CSS/JS, renderizada pelo WebView2 (nenhum Chromium embutido).
- **Transporte:** WebRTC, com servidor STUN público para funcionar tanto em
  rede local quanto pela internet.

## Desenvolvimento

Pré-requisitos: [Rust](https://www.rust-lang.org/tools/install) e
[Node.js](https://nodejs.org/).

```bash
npm install
npm run tauri dev
```
