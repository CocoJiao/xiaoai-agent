# 不登录小米账号也能下载官方固件

## 结论先说

`ota.ts` 现在必须先 `getMiNA({ did, userId, password })` 登录米家，只是因为**它需要设备序列号和当前版本**
才能拼出 OTA 地址。而这个地址本身只依赖三样东西：型号、版本、一个写死在客户端里的 secret。

所以**不需要新写一个 Go 客户端**，也不需要改 `ota.ts` 的登录逻辑：`ota.ts` 已经支持用
`OTA` 环境变量直接给地址，跳过登录。

```sh
OTA='{"url":"<下面算出来的地址>","model":"OH2P","version":"1.62.2"}' bash build.sh
```

这条路是实测过的：2026-10-24 用下面的脚本算出 OH2P 1.62.2 的地址，直接 GET 就拿到了官方固件直链
（`code=0`、`data.currentInfo.link`、`hash=bc81f5b40f3db5e9a9d5943616cd9d93`、约 36 MB），
全程没有登录任何小米账号；随后**另一个环境用同一算法独立复现**，返回的 `hash` 与大小完全一致。

> 这个 secret 是小米客户端里的公开常量，**不是账号凭据**；它只用于获取官方固件——
> 不要拿它做别的事，也不要把下载到的固件再分发。

## 地址怎么算

```python
import base64, hashlib, time

SECRET = "8007236f-a2d6-4847-ac83-c49395ad6d65"   # 客户端内置，非账号凭据

def ota_url(model: str, version: str, ms: int | None = None) -> str:
    ms = ms or int(time.time() * 1000)
    info = f"channel=release&filterID=&locale=zh_CN&model={model}&time={ms}&version={version}&{SECRET}"
    s = hashlib.md5(base64.b64encode(info.encode()).decode().encode()).hexdigest()
    return (
        f"http://api.miwifi.com/rs/grayupgrade/v2/{model}"
        f"?model={model}&version={version}&channel=release&filterID=&locale=zh_CN&time={ms}&s={s}"
    )

print(ota_url("OH2P", "1.62.2"))
```

返回里 `data.currentInfo.link` 就是固件直链，`hash` 是 MD5，`toVersion` 是版本。
`filterID`（设备序列号）留空也有效——这就是「换设备/换账号都能用」的原因。

## 为什么现在不建议引入 Go 客户端

参考实现（`cxjava/open-xiaoai`）把签名密钥内置进了 Go 版 `tools/client-patch/cmd/ota`，确实免账号。
但换到本仓库要付这些代价：

| 代价 | 说明 |
|---|---|
| 多一条构建链 | 仓库现在是 npm + Rust 两条；再加 Go 意味着 CI 多一个 job、本地多一个工具链要求 |
| 体积与维护 | 为了一个「拼 URL + 下载」的 30 行逻辑引入整个 Go 模块 |
| 收益重叠 | `OTA` 环境变量已经能完全绕过登录，改 Go 只是把「复制粘贴一段脚本」变成「跑一条命令」 |

结论：**先不引入**。用 `OTA` 覆盖 + 上面这段脚本就够；等真的有人抱怨「拼 URL 太麻烦」，
再考虑加一个 `npm run ota-url`（TS 侧 20 行，仍然不需要 Go）。

## 剩下的边界

- 只对 `channel=release` 有效；`current` / `stable` 频道未验证。
- 地址里的 `time` 参与签名，过期不失效但每次最好重新算，避免被缓存搅乱。
- `.env` 里的 `MI_DID` 仍然要填**设备名**（如 `小爱音箱Pro`），不是小米账号。
  这一条容易搞错，但**不是历史遗留 bug**：`ota.ts` 从首个提交 `e346d70` 起就是
  `did: process.env.MI_DID!`（`userId` 才是 `MI_USER`），全历史里从未出现过 `did: MI_USER`。
  验证方法：`git show e346d70:deploy/client-patch/src/ota.ts | grep -n "did:"`
  与 `git log --all -S "did: process.env.MI_USER" -- deploy/client-patch/src/ota.ts`（后者应为空）。
