# KMFlow — 局域网键鼠共享

[![CI](https://github.com/user/kmflow/actions/workflows/ci.yml/badge.svg)](https://github.com/user/kmflow/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

KMFlow 是一个轻量级的局域网键盘鼠标共享工具，类似 Synergy / Barrier，用 Rust 编写。

把鼠标移到屏幕边缘，光标就会"穿越"到另一台电脑上，键盘输入也会跟着切换。支持剪贴板同步。

## 特性

- 🖱️ **无缝切鼠标** — 鼠标移到屏幕边缘自动切换到对端
- ⌨️ **键盘跟随** — 焦点切换后键盘输入同步转发
- 📋 **剪贴板同步** — 复制的文字自动同步到对端
- 🔒 **端到端加密** — 基于 QUIC (quinn) + TLS 1.3 自签名证书
- 🔍 **自动发现** — UDP 广播发现局域网内其他节点
- 🖥️ **多后端** — 支持 X11 和 evdev（Wayland）输入
- ⚡ **低延迟** — QUIC datagram 传输，事件批处理优化
- 🔑 **TOFU 信任** — 首次连接自动信任，后续验证指纹
- 📐 **自动 Scale 检测** — 自动检测显示器 scale，精确计算逻辑分辨率用于边缘检测

## 快速开始

### 编译

```bash
# 依赖（Ubuntu/Debian）
sudo apt install libx11-dev libxtst-dev libxi-dev libxrandr-dev

# 编译
cargo build --release
```

### Wayland 环境配置（evdev 模式）

Wayland 合成器没有类似 X11 XTest 的输入注入 API，因此 KMFlow 使用 evdev/uinput 内核接口，
需要用户有 `input` 组权限：

```bash
# 加入 input 组
sudo usermod -aG input $USER

# 配置 uinput 设备权限
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules
sudo udevadm control --reload-rules
sudo udevadm trigger

# 重新登录或重启使组生效
```

配置完成后无需 sudo 即可运行 KMFlow。

> **X11 用户** 无需上述配置，KMFlow 通过 XTest 扩展注入输入，直接运行即可。

### 使用

**机器 A**（对端在右边）：
```bash
kmflow start --right
```

**机器 B**（对端在左边）：
```bash
kmflow start --left
```

两台机器启动后会通过 UDP 广播自动发现对方并连接。

也可以手动配对：
```bash
kmflow pair 192.168.1.100
```

### 其他命令

```bash
kmflow status           # 查看连接状态
kmflow stop             # 停止 daemon
kmflow setup-firewall   # 生成防火墙规则
kmflow start -v         # 启动并开启 debug 日志
```

### 紧急热键

**Ctrl + Alt + Esc** — 立即释放鼠标/键盘焦点回本机

## 架构

```
kmflow-cli          CLI 入口（clap）
  └── kmflow-daemon   核心 daemon（事件循环、会话管理、剪贴板）
        ├── kmflow-net     网络层（QUIC 传输 + UDP 发现 + TLS）
        ├── kmflow-input   输入层（X11 / evdev 捕获与模拟）
        └── kmflow-proto   协议层（类型定义 + serde_json 编解码）
```

- **输入事件** 通过 QUIC datagram 传输（无序、低延迟）
- **控制消息** 通过 QUIC 双向流传输（有序、可靠）
- **剪贴板数据** 通过独立 QUIC 流传输（支持大数据量）

## 系统要求

- Linux（X11 或 Wayland）
- Rust 1.85+
- Wayland 模式需要 `input` 用户组权限（见上方配置）
- 剪贴板同步需要 `xclip`/`xsel`（X11）或 `wl-copy`/`wl-paste`（Wayland）
- 屏幕 scale 检测依赖 `cosmic-randr`（COSMIC）或 `wlr-randr`（wlroots 系）

## 端口

| 端口 | 协议 | 用途 |
|------|------|------|
| 4242 | UDP/QUIC | 数据传输 |
| 4243 | UDP | 节点发现广播 |

## 路线图

- [ ] **libei 支持** — 使用 Wayland 原生输入注入协议（[libei](https://gitlab.freedesktop.org/libinput/libei)），
  支持后 Wayland 环境无需 `input` 组权限。等待 COSMIC 等合成器完成支持。
- [ ] 文件拖拽传输
- [ ] 多显示器布局配置 UI
- [ ] macOS / Windows 支持

## 许可证

MIT
