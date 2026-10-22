// M1 反向对拍：解开 Rust 侧加密出来的 envelope，验证「写回」这条链路。
//
// 运行方式：
//   ELECTRON_RUN_AS_NODE=1 WorkBuddy.exe tools/parity-dec.cjs <envelope>
//
// envelope 来自 `hub enc-test` 的输出字段。只输出明文哈希，不打印明文。

const crypto = require('crypto');

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

const envelope = process.argv[2];
if (!envelope) {
  console.error('用法：parity-dec.cjs <envelope>');
  process.exit(2);
}

const { key, keyId } = atRestKey();
const plain = decryptField(envelope, key, keyId);

process.stdout.write(
  JSON.stringify({
    source: 'typescript',
    plainSha256: crypto.createHash('sha256').update(plain, 'utf8').digest('hex'),
    plainLen: plain.length,
  }) + '\n',
);
