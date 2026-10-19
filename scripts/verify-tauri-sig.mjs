// 用法: node verify-tauri-sig.mjs <artifact> <artifact.sig> [pubkey-file]
// 按 minisign-verify 0.2.5 的权威格式验证 tauri signer 产出的 .sig：
//   公钥(42B) = [2B 算法][8B key_id][32B ed25519 公钥]
//   签名 bin1(74B) = [2B 算法][8B key_id][64B ed25519 签名]；末行 bin2(64B)=全局签名
//   验签: key_id 相等；算法 "ED"(0x45,0x44) 时消息体=blake2b(文件字节)，
//         "Ed"(0x45,0x64) 时为原始字节；验 ed25519(消息体) 与 ed25519(签名‖trusted_comment)。
// 退出码: 0=SIG_OK, 1=SIG_BAD, 2=格式/解析错误
import { readFileSync } from "node:fs";
import { createHash, createPublicKey, verify } from "node:crypto";

const [file, sigFile, pubFile] = process.argv.slice(2);
if (!file || !sigFile) {
  console.error("用法: node verify-tauri-sig.mjs <artifact> <artifact.sig> [pubkey-file]");
  process.exit(2);
}
const data = readFileSync(file);

// .sig 落盘为单行 base64 封装，解包后是 4 行 minisign 文本
let text = readFileSync(sigFile, "utf8").trim();
if (!text.includes("\n")) {
  const dec = Buffer.from(text, "base64").toString("utf8");
  if (dec.startsWith("untrusted comment:")) text = dec;
}
const lines = text.split(/\r?\n/).filter((l) => l.length);
if (lines.length !== 4 || !lines[0].startsWith("untrusted comment:")) {
  console.error(`签名文件形态异常（lines=${lines.length}）`);
  process.exit(2);
}
const bin1 = Buffer.from(lines[1], "base64");
const trustedComment = lines[2];
const bin2 = Buffer.from(lines[3], "base64");
if (bin1.length !== 74 || bin2.length !== 64) {
  console.error(`签名长度异常（bin1=${bin1.length} bin2=${bin2.length}）`);
  process.exit(2);
}
if (!trustedComment.startsWith("trusted comment: ")) {
  console.error("trusted comment 缺失");
  process.exit(2);
}

// 公钥：优先用 tauri.conf 的 .pub；未给则回退签名内嵌（bin1 不含公钥，无法回退）
if (!pubFile) { console.error("必须提供 pubkey-file（tauri.conf 的公钥）"); process.exit(2); }
let pubText = readFileSync(pubFile, "utf8").trim();
// .pub 可能是单行 base64 封装（tauri 生成），也可能是裸两行
if (!pubText.includes("\n") && /^[A-Za-z0-9+/=]+$/.test(pubText)) {
  pubText = Buffer.from(pubText, "base64").toString("utf8");
}
const pubLine = pubText.split(/\r?\n/).find((l) => /^[A-Za-z0-9+/=]+$/.test(l));
if (!pubLine) { console.error("公钥文件中找不到 base64 行"); process.exit(2); }
const pubB = Buffer.from(pubLine, "base64");
if (pubB.length !== 42) { console.error(`公钥长度异常（${pubB.length}，应为 42）`); process.exit(2); }

const sigKeyId = bin1.subarray(2, 10);
const pubKeyId = pubB.subarray(2, 10);
if (!sigKeyId.equals(pubKeyId)) { console.error("SIG_BAD: key_id 不匹配"); process.exit(1); }

const key = pubB.subarray(10, 42);
const signature = bin1.subarray(10, 74);
const prehashed = bin1[0] === 0x45 && bin1[1] === 0x44; // "ED"
const message = prehashed ? createHash("blake2b512").update(data).digest() : data;

const spki = Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), key]).toString("base64");
const pem = `-----BEGIN PUBLIC KEY-----\n${spki.match(/.{1,64}/g).join("\n")}\n-----END PUBLIC KEY-----\n`;
const keyObj = createPublicKey({ key: pem, format: "pem", type: "spki" });

const ok1 = verify(null, message, keyObj, signature);
const globalMsg = Buffer.concat([signature, Buffer.from(trustedComment.slice("trusted comment: ".length), "utf8")]);
const ok2 = verify(null, globalMsg, keyObj, bin2);
const ok = ok1 && ok2;
console.log(`prehashed=${prehashed} ed25519=${ok1} global=${ok2}`);
console.log(ok ? "SIG_OK" : "SIG_BAD");
process.exit(ok ? 0 : 1);
