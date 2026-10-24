# 刷机与 SSH 排障

覆盖两类问题：**刷完连不上 SSH**（上游 issue #4 就是这类）和**刷机过程本身失败**。
所有命令都是只读自查，可以放心在设备上跑。

## 一、先分清「刷坏了」和「连不上」

刷机成功的定义只有一条：**开机后能用补丁里设的 root 密码登录 SSH**。在那之前不要改启动分区、
不要重刷、不要动 `/data`。

```sh
# 1) 它在网上吗（先看路由器 DHCP 列表，或从 NAS 扫一遍）
ping -c 2 <ip>

# 2) 22 端口开了吗（squashfs 里有 dropbear，端口没开说明没起来）
nc -vz <ip> 22

# 3) 老 dropbear 只认 ssh-rsa，新 OpenSSH 默认拒绝
ssh -o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedKeyTypes=+ssh-rsa \
    -o KexAlgorithms=+diffie-hellman-group14-sha1 root@<ip>
```

三条依次失败 → 按第二节走；只有第 3 条能通 → 是客户端兼容问题，把上面的 `-o` 参数写进
`~/.ssh/config` 即可，设备没坏。

## 二、连不上时的判定顺序

1. **确认刷的是 system0 还是 system1，启动分区指向谁。** 补丁刷写顺序是
   `partition system0` → 确认写成 → `setenv boot_part boot0` → `saveenv`。`boot_part` 后置是安全设计：
   写坏时启动分区还指 `boot1`，原厂系统仍能起来（回到原厂 = SSH 连不上、但小爱能用）。
2. **开机后被原厂 OTA 冲掉了吗。** 补丁会禁用原厂 OTA（`bin/ota`、`bin/flash.sh` 里 `return 1`），
   所以补丁不会被升级覆盖；但如果之前用过社区现成的跨版本补丁包，可能已经半坏。
3. **是不是刷错了分区。** `partition system0` 相对路径、路径里不要有中文；
   刷写窗口只有 1.5–1.8 秒（上电后 `1b8e:c003` 出现到消失；来源：本项目在 NAS 上刷 OH2P 1.62.2 时实测 5 次），错过了就是「什么都没写」。
4. **确认到底写进去了什么**——用哈希比对，不要靠当时的终端记录：

```sh
# 设备上（能登录时）
md5sum /etc/shadow /etc/init.d/dropbear /bin/ota /etc/rc.local
# 本地从产物里取同名文件，逐一对照；全等即证明写入的就是这份产物
```

5. **还有救就回滚。** 把 `boot_part` 切回 `boot1`（串口或刷写工具），原厂系统启动后再重来一遍。

## 三、刷机过程本身失败

| 现象 | 原因 | 处理 |
|---|---|---|
| 只有 macOS 能构建成功，Linux/Docker 在最后一步退出 | `squashfs.sh` 用 `stat -f %z`（BSD 语法），Linux 上取不到大小 → 超限校验被静默跳过 → 算术报错退出 | **已修**（PR #9）：`gstat` / `stat -c` / `stat -f` / `wc -c` 四级回退，取不到数字直接报错 |
| 打包后设备起不来、体积明显变大 | 早先 `cp -rf $FIRMWARE assets/$FIRMWARE` 在目标已存在时会嵌套复制成 `assets/<fw>/<fw>/` | **部分缓解 / 待修**：`squashfs.sh:86` 现在先 `rm -rf` 目标目录再 `cp -rf`，嵌套已不会发生；但仍是整目录复制，旧文件会一起进 assets。待改成只复制 `root-patched.squashfs` |
| 镜像里有 `*.orig` 残留 | `patch -p1` 默认留备份 | **待修**：`patch.sh:27` 仍是 `patch -p1 < "$temp_patch"`，未加 `--no-backup-if-mismatch`。当前只能在打包前手工 `find . -name '*.orig' -delete` |
| 下载固件这一步就失败 | 需要小米账号登录；`MI_DID` 要填**设备名**（如 `小爱音箱Pro`），不是账号 | `.env` 里 `MI_USER/MI_PASS`（或 `MI_TOKEN`）+ `MI_DID`；或直接给 `OTA='{"url":...}'` 跳过登录 |
| 超级块与原厂不一致 | `mksquashfs` 参数不同 | **待修**：`squashfs.sh:31` 现在仍带 `-always-use-fragments -no-exports`。要对齐原厂需改成 `-comp xz -b 131072 -noappend -all-root -no-xattrs`（去掉这两个参数） |
| Windows 下 `update.exe` 直接 `0xC000007B` | 加载器阶段失败，原因未定位 | 换一台原生 Linux 主机刷（NAS 最方便），用 `tools/linux-x86/update` |
| usbipd 转发给 WSL 不工作 | `attach` 耗时 3.5–3.9 s > 1.5–1.8 s 烧录窗口 | 同上，走原生 Linux |

## 四、刷完先做的 4 条只读检查

```sh
uname -m                                   # 内核可能 aarch64，用户态仍是 armhf
cat /proc/mounts | grep mtdblock           # 根是 squashfs ro，说明在跑补丁固件
ls -l /tmp/mico_aivs_lab/usock/            # speech.usock / common.usock 在不在
ldd --version | head -1                    # glibc 版本，决定能用哪个 armv7 二进制
```

`/etc/inittab` 引用 `/etc/init.d/rcS` 而该文件不存在，是原厂就有的情况，**不是刷坏**。

## 五、设备是 BusyBox，别按桌面 Linux 的直觉写脚本

- 没有 `setsid`、`nohup`、`pgrep`、`od`、`strings`、`python3`；
- `timeout` 要写 `timeout -t N`；
- `scp` 必须加 `-O`（没有 sftp-server）；
- `/root` 是只读 squashfs，公钥装不进去，自动化只能从外部带密码登录；
- 每次 SSH 登录会先打一段启动横幅，`cat` 出来的内容前面会混入横幅——
  「写回后读回比较」一律夹标记取中：`echo @@B@@; cat f; echo @@E@@`。
