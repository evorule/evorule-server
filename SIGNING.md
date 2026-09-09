# 发布产物签名与验证

EvoRule Server 的 GitHub Release 分发包由 **cosign keyless 签名**（基于 OIDC，无需保管任何私钥）
生成 `.sig` 文件，随每个 Release 资产一同提供。

## 验证（以 `evorule-server-linux-x86_64` 原始二进制为例）

```bash
cosign verify-blob \
  --certificate-identity-regexp 'https://github.com/evorule/evorule-server/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  evorule-server-linux-x86_64 \
  --signature evorule-server-linux-x86_64.sig
```

- 退出码 `0` = 验证通过：该二进制确由本仓库 CI 在 `release.yml` 工作流中签名，未被篡改。
- Windows 二进制对应 `evorule-server-windows-x86_64` / `evorule-server-windows-x86_64.sig`。
- 分发包（`evorule-server-*-win64.zip` / `*-linux64.tar.gz`）另附 `sha256-checks.txt` 用于完整性校验。

## 工作原理

- 签名发生在 `release.yml` 的 `sign` job，使用 GitHub OIDC（`permissions: id-token: write`）
  向 Sigstore Fulcio 申请短期证书，**零密钥管理**。
- 证书身份绑定到本仓库的 `release.yml` 工作流，无法被其他仓库/工作流伪造同名签名。
- 验证命令的 `certificate-identity-regexp` 用 `.*` 覆盖 `refs/tags/v*` 与 `refs/heads/main`，
  请勿收紧为正则之外的固定值。

## 不签名就无法用吗？

不。签名是完整性/真实性证明，不阻断使用。验证失败仅提示你从官方 Release 重新下载。
