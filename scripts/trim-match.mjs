// 一次性辅助：对当前 git 树应用 trim-rules.json 的删除 glob，输出匹配清单。
// 用法: node scripts/trim-match.mjs <tracked-list-file> <out-file>
// node 侧不 spawn 子进程（本机 spawnSync EBUSY 环境坑），清单由调用方用 git ls-files 生成。
import fs from "node:fs";

const [trackedFile, outFile] = process.argv.slice(2);
if (!trackedFile || !outFile) {
  console.error("usage: node scripts/trim-match.mjs <tracked-list> <out-list>");
  process.exit(2);
}

const rules = JSON.parse(fs.readFileSync("scripts/trim-rules.json", "utf8"));

/** 确定性 glob->regex：** 跨段、* 单段、? 单字符、{a,b} 枚举。与 trim-app.mjs 同语义。 */
function globToRegex(g) {
  let re = "";
  for (let i = 0; i < g.length; i++) {
    const c = g[i];
    if (c === "*") {
      if (g[i + 1] === "*") {
        re += ".*";
        i++;
        if (g[i + 1] === "/") i++;
      } else {
        re += "[^/]*";
      }
    } else if (c === "?") {
      re += "[^/]";
    } else if (c === "{") {
      const j = g.indexOf("}", i);
      re += "(" + g.slice(i + 1, j).split(",").join("|") + ")";
      i = j;
    } else {
      re += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
    }
  }
  return new RegExp("^" + re + "$");
}

const regexes = rules.deletes.map(globToRegex);
const files = fs.readFileSync(trackedFile, "utf8").trim().split("\n");
const hits = files.filter((f) => regexes.some((re) => re.test(f)));
fs.writeFileSync(outFile, hits.join("\n") + (hits.length ? "\n" : ""));
console.log("matched:", hits.length);
