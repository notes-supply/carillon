import type { Limits } from "../api/stream.ts";
import { size } from "../transcript/files.ts";

export function overLimit(files: readonly File[], limits: Limits): string | undefined {
  if (files.length > limits.files)
    return `A message takes at most ${String(limits.files)} files. Remove ${String(files.length - limits.files)}.`;
  const large = files.find((file) => file.size > limits.file_bytes);
  if (large !== undefined)
    return `${large.name} is ${size(large.size)}. A file can be at most ${size(limits.file_bytes)}.`;
  const total = files.reduce((sum, file) => sum + file.size, 0);
  if (total > limits.message_bytes)
    return `These files are ${size(total)} together. A message takes at most ${size(limits.message_bytes)}.`;
  return undefined;
}
