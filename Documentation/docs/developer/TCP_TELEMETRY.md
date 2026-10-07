# SVSMからUDPでTCPログを直接送信

このブランチは `sock-trawl`（`990fc4fe`）を基点としています。SVSMが専用のvirtio-net
MMIOデバイスから、`virtio-vsock` と同じ56バイトのCTCP接続レコードを送信します。
ゲストLinuxのネットワーク処理は経由しません。

リポジトリ内の `virtio-drivers` crateが提供するMMIO、virtqueue、SVSM用のDMA・共有メモリの
ハードウェア抽象化層（HAL）を再利用します。新規に追加するのは、小さな送信専用ネットワーク
ドライバとEthernet／IPv4／UDPパケットの生成処理です。
`smoltcp` などのネットワークスタックへの依存は追加していません。

`make TCP_LOG_MODE=net` でビルドします。リポジトリの起動スクリプトを使う場合は、
`--tcp-net` を指定します。提示された `run.sh` を使う場合は、既存の `-machine` 引数に
`x-svsm-virtio-mmio=on` を追加します。

```sh
-machine q35,confidential-guest-support=sev0,memory-backend=ram1,igvm-cfg=igvm0,x-svsm-virtio-mmio=on
```

既存のゲスト用virtio-net-pciデバイスを残したまま、次のオプションを追加します。

```sh
-global virtio-mmio.force-legacy=false \
-netdev user,id=svsm_tcp \
-device virtio-net-device,netdev=svsm_tcp,mac=52:54:00:12:34:57
```

SVSMのvsock通信と同様に、Coconut QEMUの `x-svsm-virtio-mmio` 対応が必要です。
このドライバが検出するのは、fw_cfgで通知されるMMIOデバイスだけです。
専用デバイスをvirtio-net-pciに置き換えることはできません。

QEMUホストで受信スクリプトを起動します。

```sh
python3 scripts/tcp-udp-receiver.py --bind 127.0.0.1 --port 4050
```

SVSMは送信元 `10.0.2.15:4050`、宛先 `10.0.2.2:4050`、MACアドレス
`52:54:00:12:34:57` を使用します。既存の `net0` とは別のQEMUユーザーネットワークなので、
同じIPアドレス範囲を使用できます。受信スクリプトはJSONレコードを出力します。
VMからホストへの送信なので、`hostfwd` の指定は不要です。

Ethernetの宛先はブロードキャスト、IPv4の宛先はQEMUホストのアドレスです。
ARPを省く構成で、上記の専用ユーザーネットワークを前提としています。
汎用のブリッジ接続ネットワークは対象にしていません。
DHCP、受信処理、TCP、TLS、再送は実装していません。
IPv4ヘッダにはチェックサムを設定し、IPv4では省略可能なUDPチェックサムはゼロにしています。
アドレスは `kernel/src/vmm/tcp_udp.rs` 内の定数です。

modern VirtIOのネットワークヘッダは12バイトすべてゼロとし、オフロードを使いません。
受信用キュー0はバッファなしで初期化し、送信用キュー1には未完了のフレームを最大1件だけ
置きます。ドライバはデバイスによるディスクリプタの処理が完了するまで送信バッファを保持します。
ゲストからSVSMへ戻るたびに、VMSAへの参照を解放してから完了状態を確認し、待機せずに
最大32件を送信キューへ登録します。残りの処理は、次にゲストからSVSMへ戻った際に進みます。
常時ポーリングする送信タスクは設けていません。
ドライバでエラーが発生した場合は警告を出して送信を停止し、デバイスの復旧は試みません。

生成側キューは128件です。UDPでの損失、キューの容量超過、VMの停止によりレコードが
失われる場合があります。CTCPの破棄件数フィールドは、生成側キューの容量超過だけを数えます。
TCP走査のタイミング、重複の除外、ゲストLinuxのメモリ配置に関する前提は既存実装を引き継ぎます。

開発環境では、`svsm` と `stage2` を `x86_64-unknown-none` 向けにクロスビルドし、
リンクまで確認しました。`CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER` にRustツールチェーンの
`ld.lld` を指定し、`tcp-log-net` のみを有効にしています。
IGVM/vTPMを含むイメージ全体のビルドは実施していません。確認用コマンドは次のとおりです。

```sh
cargo check --locked --offline -p svsm --target x86_64-unknown-none --features tcp-log-net
cargo test --locked --offline -p virtio-drivers --lib
rustc --edition=2024 --test kernel/src/vmm/tcp_udp.rs -o /tmp/tcp-udp-tests
/tmp/tcp-udp-tests
rustc --edition=2024 --test kernel/src/vmm/tcp_event.rs -o /tmp/tcp-event-tests
/tmp/tcp-event-tests
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests -v
```

模擬デバイスを使ったテストでは、キュー内の実際のバイト列と、処理が完了するまでバッファを
保持することを確認しています。開発環境には実機のSNP VM、QEMUのネットワーク環境、
提示されたQEMUバイナリがないため、SVSMからホストまでの一連の配送はQEMUホストでの
実行確認が必要です。
