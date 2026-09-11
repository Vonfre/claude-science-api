import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export function updateManifest(version, signature, notes = "研舟应用更新") {
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) throw new Error("Invalid stable version");
  signature = signature.trim();
  if (!signature || !/^[A-Za-z0-9+/]+={0,2}$/.test(signature)) throw new Error("Invalid updater signature");
  return { version, notes, platforms: { "darwin-aarch64": {
    signature,
    url: `https://github.com/Vonfre/claude-science-api/releases/download/v${version}/SciPort_${version}_aarch64.app.tar.gz`,
  } } };
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [version, signaturePath, notesPath, outputPath] = process.argv.slice(2);
  const manifest = updateManifest(version, readFileSync(signaturePath, "utf8"), readFileSync(notesPath, "utf8"));
  writeFileSync(outputPath, JSON.stringify(manifest, null, 2) + "\n", { flag: "wx" });
}
