# Screen Streaming

Aplicativo Windows leve para transmitir a tela (ou uma janela específica) entre
computadores, com foco em baixíssimo overhead — não pode pesar o PC durante o
uso (ex: jogos rodando ao mesmo tempo).

## Funcionalidades

- Capturar a **tela inteira** ou **uma janela específica**.
- Qualidade ajustável: **resolução** (144p a 2160p/4K), **FPS** (15/30/60/120
  ou **Ilimitado**) e uma **otimização extra de FPS** opcional (prioriza a
  transmissão no processador — pode pesar mais em outros programas, por isso
  é opt-in).
- **Áudio do sistema** opcional, transmitido via Opus.
- **Vários espectadores** assistindo ao mesmo tempo com o mesmo código —
  número máximo configurável (1/2/5/10 ou **Ilimitado**), com contagem de
  quem está assistindo em tempo real.
- **Aplicar alterações** de qualidade/fonte sem derrubar quem já está
  assistindo (não precisa reconectar).
- **Tela cheia** e **picture-in-picture** (janela nativa, sempre no topo).
- Não precisa hospedar/configurar nenhum servidor à parte — quem transmite já
  sobe um servidor de sinalização embutido automaticamente, só compartilha um
  endereço e um código.

## Instalação

Requisitos para **usar** o app (não confundir com os pré-requisitos de
desenvolvimento mais abaixo, que só valem para quem for compilar o
código-fonte):

- Windows 10/11.
- GPU com suporte a encoding de H.264 por hardware (NVIDIA NVENC, AMD AMF,
  Intel QuickSync, ou o encoder genérico do Media Foundation do próprio
  Windows como último recurso) — hoje não existe um caminho de encoding por
  software na transmissão real, só o de hardware. Praticamente qualquer PC
  Windows 10/11 recente atende isso, mesmo sem placa de vídeo dedicada.

Passo a passo:

1. Baixe o instalador (`screen_streaming_<versão>_x64-setup.exe`) e rode-o.
2. O Windows SmartScreen pode avisar "O Windows protegeu o computador" por o
   instalador não ser assinado digitalmente — clique em "Mais informações" e
   depois "Executar assim mesmo".
3. Pronto — não é preciso instalar mais nada separadamente. O instalador já
   cuida de tudo que o app precisa em tempo de execução:
   - As **DLLs do FFmpeg** (encoding por hardware) vêm embutidas no próprio
     instalador.
   - O **Opus** (áudio) está compilado direto dentro do executável — nenhum
     arquivo separado.
   - O **Visual C++ Redistributable** é instalado automaticamente nos
     bastidores, mas só se a máquina ainda não tiver um (a maioria já tem).
   - O **WebView2 Runtime** (motor que desenha a interface) já vem no
     Windows 11; no Windows 10 sem ele, o próprio instalador baixa e instala
     — só precisa de internet nesse passo específico.
4. Se o app não abrir depois de instalado reclamando de alguma DLL faltando,
   o motivo mais comum é o antivírus removendo por engano um dos arquivos de
   codec (falso positivo conhecido com nomes como "avcodec") — adicione uma
   exceção para a pasta de instalação e instale de novo.

## Como usar

### Transmitir sua tela

1. Abra o app na aba **Transmitir**.
2. Em **Fonte**, escolha "Tela inteira" ou "Uma janela específica" (busque
   pelo nome da janela/programa na lista).
3. Em **Qualidade**, ajuste resolução, FPS (ou "Ilimitado (se o computador
   aguentar)"), a "Otimização extra de FPS" se quiser espremer mais
   performance mesmo pesando mais no PC, e se quer transmitir o áudio do
   sistema também.
4. Escolha o **número máximo de espectadores** (um número fixo, ou
   "Ilimitado").
5. Clique em **Iniciar transmissão**. A tela vai mostrar um ou mais
   **endereços** (um por interface de rede desta máquina) e um **código de
   pareamento**.
6. Compartilhe o **endereço** e o **código** com quem for assistir — os dois
   são necessários (ver "Requisitos de rede" abaixo).
7. Assim que a primeira pessoa conectar, a transmissão começa de verdade.
   Outras pessoas podem entrar com o mesmo código depois, até o limite
   escolhido. Pode mudar qualquer configuração de qualidade a qualquer
   momento e clicar em **Aplicar alterações** sem que quem já está assistindo
   precise reconectar.
8. **Parar transmissão** encerra para todo mundo que estiver assistindo.

### Assistir a uma transmissão

1. Peça a quem for transmitir o **endereço do servidor** (`ws://IP:9876`) e o
   **código de pareamento**.
2. Na aba **Assistir**, cole os dois em "Servidor de sinalização" e "Código
   de pareamento" e clique em **Conectar**.
3. Use **Tela cheia** ou **Picture-in-picture** (janela separada,
   sempre-no-topo) para acompanhar, e o controle de **volume** se a
   transmissão tiver áudio.
4. **Sair** encerra sua conexão (a transmissão continua normalmente para
   quem mais estiver assistindo).

### Requisitos de rede

As duas máquinas precisam conseguir se alcançar pelo endereço mostrado —
mesma rede local, uma VPN tipo Radmin/Hamachi/Tailscale, ou
port-forward/IP público. Na primeira vez que a transmissão aceitar uma
conexão de outra máquina, o Firewall do Windows pode pedir permissão — é
preciso permitir, senão quem estiver em outra máquina não consegue
conectar. O código de pareamento (6 dígitos) é o controle de acesso real:
só quem recebe endereço **e** código consegue entrar numa transmissão.

## Stack

- **Backend:** Rust (via [Tauri](https://tauri.app)) — captura de tela e
  codificação de vídeo com o mínimo de overhead possível.
- **UI:** HTML/CSS/JS, renderizada pelo WebView2 (nenhum Chromium embutido).
- **Transporte:** WebRTC (P2P, sem a tela passar por nenhum servidor), com
  servidor STUN público para funcionar tanto em rede local quanto pela
  internet, e um servidor de sinalização (troca inicial de conexão) embutido
  no próprio app de quem transmite.

## Desenvolvimento

Tudo nesta seção só é necessário para **compilar o código-fonte** — quem só
vai usar o app instala o `.exe` da seção "Instalação" acima e não precisa de
nada disso.

Pré-requisitos:

- [Rust](https://www.rust-lang.org/tools/install) (o instalador do `rustup`
  no Windows já oferece para instalar o linker/Build Tools da Visual Studio
  que o toolchain padrão precisa — aceite essa opção) e
  [Node.js](https://nodejs.org/).
- LLVM/libclang — necessário para o `bindgen` de algumas dependências
  nativas (ex: FFmpeg).
- As **DLLs do FFmpeg** (build LGPL "shared", mesma versão major usada em
  `src-tauri/Cargo.toml` — `ffmpeg-next = "9"`) — necessárias em tempo de
  execução para o encoder de hardware funcionar. Deixe-as em algum diretório
  no `PATH`, ou copie para `src-tauri/ffmpeg-bin/*.dll` (pasta gitignored,
  precisa criar manualmente — é também de onde `npm run build` empacota as
  DLLs no instalador).

Só necessário se for gerar o instalador (`npm run build`), não para
`npm run tauri dev`:

- O instalador oficial do **Visual C++ Redistributable x64**
  ([aka.ms/vs/17/release/vc_redist.x64.exe](https://aka.ms/vs/17/release/vc_redist.x64.exe))
  salvo em `src-tauri/vcredist/vc_redist.x64.exe` (pasta gitignored, mesma
  ideia da `ffmpeg-bin/`) — `npm run build` empacota esse arquivo no
  instalador, que passa a instalá-lo automaticamente em quem não tiver (ver
  `nsis-hooks.nsh`).

```bash
npm install
npm run tauri dev
```

Testes: `cargo test` (dentro de `src-tauri/` ou `signaling-server/`) roda os
testes rápidos/mockados; `cargo test -- --ignored --nocapture` roda os
testes reais, que precisam de tela/GPU/rede de verdade.

Build do instalador (Windows, NSIS): `npm run build` (equivalente a
`npm run tauri build`) — gera o instalador em
`src-tauri/target/release/bundle/nsis/`.
