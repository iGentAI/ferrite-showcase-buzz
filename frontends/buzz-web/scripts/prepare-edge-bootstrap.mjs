import { readFile, writeFile } from "node:fs/promises";

const outputDir = new URL("../dist-edge/", import.meta.url);
const indexPath = new URL("index.html", outputDir);
let index = await readFile(indexPath, "utf8");

const entryMatch = index.match(
  /<script type="module" crossorigin src="(\/buzz\/ui\/assets\/index-[^"]+\.js)"><\/script>/,
);
if (!entryMatch) throw new Error("edge index does not contain one Vite entry script");

const entryUrl = entryMatch[1];
const bootstrap = `<script type="module">
      const entryUrl = ${JSON.stringify(entryUrl)};
      const start = () => import(entryUrl);

      if (!("serviceWorker" in navigator)) {
        await start();
      } else if (navigator.serviceWorker.controller) {
        await start();
      } else {
        try {
          await navigator.serviceWorker.register("/buzz/ui/asset-gate-sw.js", {
            scope: "/",
          });
          await navigator.serviceWorker.ready;
          window.location.reload();
        } catch (error) {
          console.error("Buzz asset gate could not start", error);
          await start();
        }
      }
    </script>`;

index = index
  .replace(entryMatch[0], bootstrap)
  .replace(/\s*<link rel="modulepreload"[^>]+>/g, "")
  .replace(
    /href="\/buzz\/ui\/buzz\.svg\?v=[^"]+"/,
    'href="/favicon.ico"',
  );

if (index.includes('rel="modulepreload"')) {
  throw new Error("edge index still contains an initial module preload");
}
if (index.includes("/buzz/ui/buzz.svg")) {
  throw new Error("edge index still contains an authenticated favicon request");
}

await writeFile(indexPath, index);
console.log(`installed bounded authenticated edge bootstrap for ${entryUrl}`);
