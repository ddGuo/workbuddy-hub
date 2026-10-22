// M1 对拍基准：用客户端自带的 Node 运行时解密凭据，输出哈希。
//
// 运行方式（必须用 WorkBuddy 自己的可执行文件，才能拿到 linked binding）：
//   WorkBuddy.exe tools/parity.cjs
// 环境变量：ELECTRON_RUN_AS_NODE=1
//
// 只输出 SHA-256，不输出明文 —— 对拍只需要确认两边结果一致，
// 不需要把 token 打到终端里。

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

function authPath() {
  const roots = [process.env.LOCALAPPDATA, process.env.APPDATA].filter(Boolean);
  for (const r of roots) {
    const p = path.join(r, 'CodeBuddyExtension', 'Data', 'Public', 'auth', 'workbuddy-desktop.info');
    if (fs.existsSync(p)) return p;
  }
  throw new Error('找不到 workbuddy-desktop.info');
}

function atRestKey() {
  const raw = String(process._linkedBinding('electron_browser_workbuddy_storage').loggerGet());
  const payload = JSON.parse(raw);
  if (!payload || typeof payload.atRestSecretKey !== 'string') {
    throw new Error('输出中缺少 atRestSecretKey');
  }
  const key = crypto.createHash('sha256').update(payload.atRestSecretKey, 'utf8').digest();
  const keyId = crypto.createHash('sha256').update(key).digest('hex').slice(0, 16);
  return { key, keyId };
}

function lenPrefix(value) {
  const bytes = Buffer.from(value, 'utf8');
  const header = Buffer.allocUnsafe(4);
  header.writeUInt32BE(bytes.length);
  return Buffer.concat([header, bytes]);
}

function buildAad(suite, keyId) {
  const suiteBytes = Buffer.allocUnsafe(4);
  suiteBytes.writeUInt32BE(suite);
  return Buffer.concat([
    Buffer.from('WB-AAD\0', 'ascii'),
    Buffer.from([1]),
    lenPrefix('WBEV1'),
    lenPrefix('sym-v1'),
    suiteBytes,
    lenPrefix(keyId),
    Buffer.from([2]),
    Buffer.from([0]),
    Buffer.from([0]),
  ]);
}

function decryptField(envelopeB64, key, localKeyId) {
  const parts = JSON.parse(Buffer.from(envelopeB64, 'base64').toString('utf8'));
  if (parts.keyId !== localKeyId) throw new Error('keyId mismatch');
  const aad = buildAad(parts.suite, parts.keyId);
  const decipher = crypto.createDecipheriv(
    'aes-256-gcm',
    key,
    Buffer.from(parts.nonce, 'base64'),
    { authTagLength: 16 },
  );
  decipher.setAAD(aad);
  decipher.setAuthTag(Buffer.from(parts.authTag, 'base64'));
  return Buffer.concat([
    decipher.update(Buffer.from(parts.ciphertext, 'base64')),
    decipher.final(),
  ]).toString('utf8');
}

function valueOf(field, key, keyId) {
  if (typeof field === 'string') return field;
  if (field && field.$wbEncrypted === 1) return decryptField(field.envelope, key, keyId);
  return null;
}

function sha256(s) {
  return crypto.createHash('sha256').update(s, 'utf8').digest('hex');
}

const p = authPath();
const doc = JSON.parse(fs.readFileSync(p, 'utf8'));
const { key, keyId } = atRestKey();

const access = valueOf(doc.auth.accessToken, key, keyId);
const refresh = valueOf(doc.auth.refreshToken, key, keyId);

process.stdout.write(
  JSON.stringify({
    source: 'typescript',
    uid: doc.account && doc.account.uid,
    keyId,
    aadLen: buildAad(1, keyId).length,
    accessLen: access ? access.length : 0,
    refreshLen: refresh ? refresh.length : 0,
    accessSha256: access ? sha256(access) : null,
    refreshSha256: refresh ? sha256(refresh) : null,
  }) + '\n',
);
