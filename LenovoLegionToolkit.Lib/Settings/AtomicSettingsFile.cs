using System;
using System.Collections.Concurrent;
using System.IO;
using System.Text;

namespace LenovoLegionToolkit.Lib.Settings;

internal enum SettingsWriteStage { BeforeTemporaryWrite, AfterTemporaryWrite, BeforeCommit, AfterCommit }

// Cooperating in-process readers and writers share a path lock. Windows replacement
// can temporarily make a name unavailable to a racing open; do not expose that gap
// as a missing configuration. Temporary files and .previous stay on the same volume.
internal static class AtomicSettingsFile
{
    private static readonly ConcurrentDictionary<string, object> FileLocks = new(StringComparer.OrdinalIgnoreCase);

    internal static string ReadSnapshot(string path)
    {
        var target = Path.GetFullPath(path);
        lock (FileLocks.GetOrAdd(target, _ => new object()))
        {
            using var stream = new FileStream(target, FileMode.Open, FileAccess.Read,
                FileShare.ReadWrite | FileShare.Delete);
            using var reader = new StreamReader(stream, Encoding.UTF8, detectEncodingFromByteOrderMarks: true);
            return reader.ReadToEnd();
        }
    }

    internal static void Write(string path, string contents, Action<SettingsWriteStage>? checkpoint = null)
    {
        var target = Path.GetFullPath(path);
        var directory = Path.GetDirectoryName(target) ?? throw new ArgumentException("Settings path needs a parent directory.", nameof(path));
        lock (FileLocks.GetOrAdd(target, _ => new object()))
        {
            Directory.CreateDirectory(directory);
            var temporary = Path.Combine(directory, "." + Path.GetFileName(target) + "." + Guid.NewGuid().ToString("N") + ".pending");
            bool created = false;
            try
            {
                checkpoint?.Invoke(SettingsWriteStage.BeforeTemporaryWrite);
                using (var stream = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None,
                           bufferSize: 4096, FileOptions.WriteThrough))
                {
                    created = true;
                    var bytes = new UTF8Encoding(encoderShouldEmitUTF8Identifier: false).GetBytes(contents);
                    stream.Write(bytes);
                    stream.Flush(flushToDisk: true);
                }
                checkpoint?.Invoke(SettingsWriteStage.AfterTemporaryWrite);
                checkpoint?.Invoke(SettingsWriteStage.BeforeCommit);
                if (File.Exists(target))
                    File.Replace(temporary, target, target + ".previous", ignoreMetadataErrors: true);
                else
                    File.Move(temporary, target);
                checkpoint?.Invoke(SettingsWriteStage.AfterCommit);
            }
            finally
            {
                if (created && File.Exists(temporary)) File.Delete(temporary);
            }
        }
    }
}
