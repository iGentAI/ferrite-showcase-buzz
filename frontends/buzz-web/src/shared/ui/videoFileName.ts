export function fileNameFromUrl(src: string): string {
  const withoutQueryOrHash = src.split(/[?#]/, 1)[0] ?? "";
  const pathPart = withoutQueryOrHash.replace(
    /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/[^/]*/,
    "",
  );
  const tail = pathPart.split("/").filter(Boolean).pop();
  if (!tail) return "Video";
  try {
    return decodeURIComponent(tail);
  } catch {
    return tail;
  }
}
