// Checks the winget and Scoop files under packaging/ against each other and Cargo.toml.
//   bun scripts/check-packaging.ts            structure only; a placeholder hash is a warning
//   bun scripts/check-packaging.ts --release  also fails on a placeholder hash (run before a submission)
// Nothing here talks to the network or submits anything.

import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "..");
const release = process.argv.includes("--release");
const errors: string[] = [];
const warnings: string[] = [];
const PLACEHOLDER = "0".repeat(64);

const cargo = readFileSync(join(root, "Cargo.toml"), "utf8");
const version = /\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/.exec(cargo)?.[1];
if (!version) throw new Error("no workspace version in Cargo.toml");

const asset = `chatcli-v${version}-x86_64-pc-windows-msvc`;
const url = `https://github.com/Page011/chatCLI/releases/download/v${version}/${asset}.zip`;

function expect(cond: boolean, msg: string) {
  if (!cond) errors.push(msg);
}

function hashOk(h: string | undefined, where: string) {
  expect(!!h && /^[0-9a-f]{64}$/.test(h), `${where}: hash is not 64 lowercase hex digits`);
  if (h === PLACEHOLDER) {
    (release ? errors : warnings).push(`${where}: placeholder hash (fill it from the release .sha256 file)`);
  }
}

// ---- winget: a folder per version holding three manifests -------------------------------
const wingetDir = join(root, "packaging/winget/manifests/p/Page011/chatcli", version);
let files: string[] = [];
try {
  files = readdirSync(wingetDir);
} catch {
  errors.push(`winget: no manifest folder for version ${version} (${wingetDir})`);
}
for (const name of ["Page011.chatcli.yaml", "Page011.chatcli.installer.yaml", "Page011.chatcli.locale.en-US.yaml"]) {
  expect(files.includes(name), `winget: ${name} is missing`);
  if (!files.includes(name)) continue;
  const text = readFileSync(join(wingetDir, name), "utf8");
  expect(/^PackageIdentifier: Page011\.chatcli$/m.test(text), `${name}: PackageIdentifier must be Page011.chatcli`);
  expect(new RegExp(`^PackageVersion: ${version.replaceAll(".", "\.")}$`, "m").test(text), `${name}: PackageVersion is not ${version}`);
  expect(/^ManifestVersion: 1\.6\.0$/m.test(text), `${name}: ManifestVersion must be 1.6.0`);
  if (name.endsWith(".installer.yaml")) {
    expect(/^InstallerType: zip$/m.test(text), `${name}: InstallerType must be zip`);
    expect(/^NestedInstallerType: portable$/m.test(text), `${name}: NestedInstallerType must be portable`);
    const rel = `${asset}${"\\"}chatcli.exe`;
    expect(text.includes(`RelativeFilePath: ${rel}`), `${name}: RelativeFilePath must be ${rel}`);
    expect(/PortableCommandAlias: chatcli$/m.test(text), `${name}: PortableCommandAlias must be chatcli`);
    expect(text.includes(`InstallerUrl: ${url}`), `${name}: InstallerUrl must be ${url}`);
    hashOk(/^\s*InstallerSha256: (\S+)$/m.exec(text)?.[1], name);
  }
}

// ---- Scoop ---------------------------------------------------------------------------
const scoop = JSON.parse(readFileSync(join(root, "packaging/scoop/bucket/chatcli.json"), "utf8"));
expect(scoop.version === version, `scoop: version ${scoop.version} is not ${version}`);
expect(scoop.architecture?.["64bit"]?.url === url, "scoop: url does not match the release asset");
hashOk(scoop.architecture?.["64bit"]?.hash, "scoop");
expect(scoop.extract_dir === asset, `scoop: extract_dir must be ${asset}`);
expect(scoop.bin === "chatcli.exe", "scoop: bin must be chatcli.exe");
expect(scoop.checkver === "github", "scoop: checkver must be github");
const au = scoop.autoupdate?.architecture?.["64bit"];
expect(au?.url === url.replaceAll(version, "$version"), "scoop: autoupdate url must be the asset url with $version");
expect(au?.hash?.url === "$url.sha256", "scoop: autoupdate must read the hash from $url.sha256");
expect(scoop.autoupdate?.extract_dir === asset.replaceAll(version, "$version"), "scoop: autoupdate extract_dir must use $version");

for (const w of warnings) console.log(`warning: ${w}`);
for (const e of errors) console.log(`error: ${e}`);
console.log(errors.length ? `${errors.length} error(s).` : "packaging files are consistent.");
process.exit(errors.length ? 1 : 0);
