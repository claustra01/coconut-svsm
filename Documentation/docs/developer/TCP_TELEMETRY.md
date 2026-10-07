# 共有メモリによるTCPログの取り出し

このブランチは `sock-trawl`（`990fc4fe`）を基点としています。既存のTCP走査で
観測した接続情報を、ホストと共有する16 KiBのリングバッファに書き込みます。
ホスト側の収集スクリプトは、QEMUのQMPコマンド `pmemsave` でリングの内容を
コピーして読み取ります。QEMUのソース変更、ゲストLinux用ドライバ、vsockデバイスは
不要です。この実装は定期的なコピーによる試作であり、ホストからのゼロコピー参照には
対応していません。

`make TCP_LOG_MODE=shmem` でビルドします。リポジトリの起動スクリプトを使う場合は、
`--tcp-shmem-qmp /tmp/svsm-tcp-qmp.sock` を指定します。

既存の `run.sh` を使う場合は、`-serial mon:stdio` を残したまま次を追加します。

```sh
-qmp unix:/tmp/svsm-tcp-qmp.sock,server=on,wait=off
```

最初のTCP接続を観測すると、SVSMは次のメッセージを出力します。

```text
TCP shared ring: gpa=0x... size=16384
```

QEMUを実行しているホストで、表示されたGPA（ゲスト物理アドレス）を指定します。

```sh
sudo python3 scripts/tcp-shmem-relay.py \
  --qmp /tmp/svsm-tcp-qmp.sock --gpa 0xADDRESS
```

収集スクリプトは、接続ごとに1つのJSONオブジェクトを出力します。QEMUと収集スクリプトは
同じホスト上で実行し、QEMUからスクリプトの一時ディレクトリにアクセスできる必要が
あります。読み取り間隔は既定で200 msです（`--interval` で変更可能）。
VMを再起動した場合は、新しく表示されたGPAを指定して収集スクリプトを起動し直します。

先頭64バイトのヘッダには、`CTCPRNG1` と4つのリトルエンディアンu32値を格納します。
順に、確保した領域のバイト数、スロット数（128）、スロットのバイト数（72）、
レコードのバイト数（56）です。各スロットは、リトルエンディアンu64の連番、
CTCPレコード、同じ連番の順に並びます。書き手は両端の連番をゼロにしてからレコードを
更新し、最後に新しい連番を書き込みます。読み手は更新途中のスロットと取得済みの連番を
読み飛ばします。

リングは一周すると古いスロットを上書きします。受信確認や再送は行いません。
連番の欠落は、スロットの上書き、または128件の生成側キューが満杯になったことを示す
場合があります。レコード内の破棄件数は、生成側キューの容量超過だけを数えます。

レコード形式は `virtio-vsock` と同じ56バイトのCTCPです。識別子、バージョン、種別、
長さ、連番、観測時のTSC、ソケットのアドレス、TCP状態、送信元と宛先のIPv4アドレス・
ポート、破棄件数を含みます。ホストとの共有メモリ上では平文です。

開発環境では、`svsm` と `stage2` を `x86_64-unknown-none` 向けにクロスビルドし、
リンクまで確認しました。`CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER` にRustツールチェーンの
`ld.lld` を指定し、`tcp-log-shmem` のみを有効にしています。
IGVM/vTPMを含むイメージ全体のビルドは実施していません。確認用コマンドは次のとおりです。

```sh
cargo check --locked --offline -p svsm --target x86_64-unknown-none --features tcp-log-shmem
rustc --edition=2024 --test kernel/src/vmm/tcp_event.rs -o /tmp/tcp-event-tests
/tmp/tcp-event-tests
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests -v
```

実際のSEV-SNP環境での動作と、QMPによる共有ページの読み取りは、QEMUホストでの確認が
必要です。TCP走査の間隔と、ゲストカーネルのメモリ配置に関する前提は既存実装を引き継ぎます。
