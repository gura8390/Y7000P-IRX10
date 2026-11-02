using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Settings;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

public static class AtomicSettingsRegressionTests
{
    public static async Task<int> RunAsync()
    {
        var checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new InvalidOperationException(name); checks++; }
        var root = Path.Combine(Path.GetTempPath(), "Y7000p-AtomicSettings-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        string DirectoryFor(string name)
        {
            var directory = Path.Combine(root, name);
            Directory.CreateDirectory(directory);
            return directory;
        }
        static bool NoPending(string directory) => Directory.GetFiles(directory, "*.pending").Length == 0;
        try
        {
            var stages = Enum.GetValues<SettingsWriteStage>();
            var oldest = Document(0, 0);
            var old = Document(0, 1);
            var next = Document(1, 2);
            foreach (var stage in stages)
            {
                var directory = DirectoryFor("existing-" + stage);
                var path = Path.Combine(directory, "settings.json");
                AtomicSettingsFile.Write(path, oldest);
                AtomicSettingsFile.Write(path, old);
                var injected = new IOException("fixture checkpoint " + stage);
                var visited = new List<SettingsWriteStage>();
                Exception? failure = null;
                try
                {
                    AtomicSettingsFile.Write(path, next, current =>
                    {
                        visited.Add(current);
                        if (current == stage) throw injected;
                    });
                }
                catch (Exception ex) { failure = ex; }
                var committed = stage == SettingsWriteStage.AfterCommit;
                Check(ReferenceEquals(failure, injected), $"Existing-file {stage} failure reaches the caller unchanged");
                Check(visited.SequenceEqual(stages.Take(Array.IndexOf(stages, stage) + 1)),
                    $"Existing-file {stage} checkpoint order reaches exactly the injected boundary");
                var actual = AtomicSettingsFile.ReadSnapshot(path);
                Check(actual == (committed ? next : old) && ValidDocument(actual),
                    $"Existing-file {stage} failure leaves the complete old or committed new JSON");
                Check(AtomicSettingsFile.ReadSnapshot(path + ".previous") == (committed ? old : oldest),
                    $"Existing-file {stage} failure preserves the correct previous complete document");
                Check(NoPending(directory), $"Existing-file {stage} failure removes every temporary pending file");
            }

            foreach (var stage in stages)
            {
                var directory = DirectoryFor("new-" + stage);
                var path = Path.Combine(directory, "settings.json");
                var injected = new IOException("fixture first-write checkpoint " + stage);
                var visited = new List<SettingsWriteStage>();
                Exception? failure = null;
                try
                {
                    AtomicSettingsFile.Write(path, next, current =>
                    {
                        visited.Add(current);
                        if (current == stage) throw injected;
                    });
                }
                catch (Exception ex) { failure = ex; }
                var committed = stage == SettingsWriteStage.AfterCommit;
                Check(ReferenceEquals(failure, injected), $"First-write {stage} failure reaches the caller unchanged");
                Check(visited.SequenceEqual(stages.Take(Array.IndexOf(stages, stage) + 1)),
                    $"First-write {stage} checkpoint order reaches exactly the injected boundary");
                Check(committed ? AtomicSettingsFile.ReadSnapshot(path) == next : !File.Exists(path),
                    $"First-write {stage} failure leaves either no target or the complete committed document");
                Check(!File.Exists(path + ".previous"), $"First-write {stage} never invents a previous document");
                Check(NoPending(directory), $"First-write {stage} failure removes every temporary pending file");
            }

            {
                var directory = DirectoryFor("successful-rotation");
                var path = Path.Combine(directory, "settings.json");
                AtomicSettingsFile.Write(path, oldest);
                AtomicSettingsFile.Write(path, old);
                AtomicSettingsFile.Write(path, next);
                Check(AtomicSettingsFile.ReadSnapshot(path) == next, "Successful replacement publishes the exact complete Unicode document");
                Check(AtomicSettingsFile.ReadSnapshot(path + ".previous") == old,
                    "Repeated successful replacement rotates previous to the immediately preceding document");
                var bytes = File.ReadAllBytes(path);
                Check(bytes.Length > 4096 && !(bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF),
                    "Atomic write persists a multi-buffer UTF-8 document without adding a BOM");
                Check(NoPending(directory), "Successful repeated replacement removes every pending file");
            }

            {
                var directory = DirectoryFor("real-commit-sharing-failure");
                var path = Path.Combine(directory, "settings.json");
                AtomicSettingsFile.Write(path, old);
                Exception? failure = null;
                using (var held = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read))
                {
                    try { AtomicSettingsFile.Write(path, next); }
                    catch (Exception ex) { failure = ex; }
                    Check(failure is IOException or UnauthorizedAccessException,
                        "A real target handle without delete sharing rejects the atomic commit");
                    Check(AtomicSettingsFile.ReadSnapshot(path) == old,
                        "A real commit sharing failure leaves the original target complete and readable");
                    Check(NoPending(directory), "A real commit sharing failure removes the completed pending file");
                }
                AtomicSettingsFile.Write(path, next);
                Check(AtomicSettingsFile.ReadSnapshot(path) == next && AtomicSettingsFile.ReadSnapshot(path + ".previous") == old
                    && NoPending(directory), "A rejected commit can be retried successfully after the incompatible reader releases its handle");
            }

            {
                var directory = DirectoryFor("concurrent-snapshots");
                var path = Path.Combine(directory, "settings.json");
                AtomicSettingsFile.Write(path, Document(0, 0));
                const int writerCount = 4;
                const int writesPerWriter = 24;
                const int readerCount = 2;
                var remainingWriters = writerCount;
                var writeCount = 0;
                var readCount = 0;
                var readerStarted = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
                var readers = Enumerable.Range(0, readerCount).Select(_ => Task.Run(async () =>
                {
                    var localReads = 0;
                    readerStarted.TrySetResult();
                    while (Volatile.Read(ref remainingWriters) > 0 || localReads < 10)
                    {
                        var snapshot = AtomicSettingsFile.ReadSnapshot(path);
                        if (!ValidDocument(snapshot))
                            throw new InvalidOperationException("A concurrent reader observed mixed or incomplete JSON.");
                        Interlocked.Increment(ref readCount);
                        localReads++;
                        await Task.Yield();
                    }
                })).ToArray();
                var writers = Enumerable.Range(0, writerCount).Select(writer => Task.Run(async () =>
                {
                    try
                    {
                        await readerStarted.Task;
                        for (var sequence = 1; sequence <= writesPerWriter; sequence++)
                        {
                            AtomicSettingsFile.Write(path, Document(writer, sequence));
                            Interlocked.Increment(ref writeCount);
                        }
                    }
                    finally { Interlocked.Decrement(ref remainingWriters); }
                })).ToArray();
                await Task.WhenAll(readers.Concat(writers));
                Check(writeCount == writerCount * writesPerWriter, "All concurrent atomic writers commit without sharing violations");
                Check(readCount >= readerCount * 10, "Concurrent ReadSnapshot readers repeatedly observe complete known JSON documents");
                Check(ValidDocument(AtomicSettingsFile.ReadSnapshot(path)), "The final concurrent target remains a complete writer document");
                Check(ValidDocument(AtomicSettingsFile.ReadSnapshot(path + ".previous")), "The final concurrent previous file remains a complete writer document");
                Check(NoPending(directory), "Concurrent replacements leave no pending temporary files");
            }

            {
                var directory = DirectoryFor("serialization-failure");
                var path = Path.Combine(directory, "settings.json");
                var settings = new FaultingSettings(directory);
                settings.SynchronizeStoreOrThrow();
                settings.Store.Revision = 1;
                settings.SynchronizeStoreOrThrow();
                var original = AtomicSettingsFile.ReadSnapshot(path);
                var previous = AtomicSettingsFile.ReadSnapshot(path + ".previous");
                settings.Store.FailSerialization = true;
                Exception? failure = null;
                try { settings.SynchronizeStoreOrThrow(); }
                catch (Exception ex) { failure = ex; }
                Check(failure is JsonSerializationException && failure.ToString().Contains("fixture serializer failed", StringComparison.Ordinal),
                    "Critical AbstractSettings persistence reports a serializer failure before file mutation");
                Check(AtomicSettingsFile.ReadSnapshot(path) == original, "A serializer failure leaves the target byte-for-byte unchanged");
                Check(AtomicSettingsFile.ReadSnapshot(path + ".previous") == previous,
                    "A serializer failure leaves the previous snapshot byte-for-byte unchanged");
                Check(NoPending(directory), "A serializer failure creates no temporary pending file");
                settings.Save();
                settings.SynchronizeStore();
                Check(AtomicSettingsFile.ReadSnapshot(path) == original && AtomicSettingsFile.ReadSnapshot(path + ".previous") == previous
                    && NoPending(directory), "Legacy Save and SynchronizeStore contain serializer failures without modifying either snapshot");
            }

            {
                var directory = DirectoryFor("backup-original-read-snapshot");
                var path = Path.Combine(directory, "settings.json");
                const string damaged = "{\r\n  \"Revision\": not-json,\r\n  \"Name\": \"损坏原文😀\"\r\n}";
                const string replacement = "{\"Revision\":91,\"Name\":\"replacement fixture\"}";
                File.WriteAllText(path, damaged);
                var settings = new ReplacingOnDeserializeSettings(directory, replacement);
                Check(settings.LoadStore() is null, "AbstractSettings rejects the snapshot when its production deserialize hook throws");
                Check(settings.ObservedSnapshot == damaged, "The deserialize hook receives the exact original damaged read snapshot");
                Check(AtomicSettingsFile.ReadSnapshot(path) == replacement,
                    "The deterministic deserialize race actually replaces the current target before backup begins");
                var backups = Directory.GetFiles(directory, "settings_backup_*.json");
                Check(backups.Length == 1 && AtomicSettingsFile.ReadSnapshot(backups[0]) == damaged,
                    "A failed load backs up its original damaged snapshot rather than a newer replacement file");
                Check(NoPending(directory), "Backing up a raced damaged snapshot leaves no pending file");
            }

            {
                var directory = DirectoryFor("rapid-distinct-corrupt-backups");
                var path = Path.Combine(directory, "settings.json");
                const string damaged = "{\n  \"Revision\": repeated-bad-json,\n  \"Name\": \"同秒损坏备份😀\"\n}";
                File.WriteAllText(path, damaged);
                const int callers = 12;
                using var start = new ManualResetEventSlim();
                using var ready = new CountdownEvent(callers);
                var settings = new ProbeSettings(directory);
                var loads = Enumerable.Range(0, callers).Select(_ => Task.Factory.StartNew(() =>
                {
                    ready.Signal();
                    start.Wait();
                    return settings.LoadStore();
                }, CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default)).ToArray();
                var allReady = ready.Wait(TimeSpan.FromSeconds(5));
                start.Set();
                var results = await Task.WhenAll(loads);
                Check(allReady && results.All(store => store is null), "Rapid concurrent malformed loads all reject the same synthetic damaged document");
                var backups = Directory.GetFiles(directory, "settings_backup_*.json");
                Check(backups.Length == callers && backups.Distinct(StringComparer.OrdinalIgnoreCase).Count() == callers,
                    "Every rapid malformed load creates its own backup without overwriting another");
                Check(backups.All(backup => AtomicSettingsFile.ReadSnapshot(backup) == damaged),
                    "Every rapid malformed-load backup contains the complete original damaged document");
                const int prefixLength = 16; // "settings_backup_" followed by a 14-character UTC-second prefix.
                Check(backups.GroupBy(backup => Path.GetFileName(backup).Substring(prefixLength, 14)).Any(group => group.Count() > 1),
                    "The backup uniqueness test actually exercises repeated backups within the same UTC second");
                Check(NoPending(directory), "Rapid same-second corrupt backups leave no pending files");
            }

            {
                var directory = DirectoryFor("concurrent-store-initialization");
                File.WriteAllText(Path.Combine(directory, "settings.json"), "{\"Revision\":77,\"Name\":\"loaded fixture\"}");
                using var start = new ManualResetEventSlim();
                using var loadEntered = new ManualResetEventSlim();
                using var allowLoad = new ManualResetEventSlim();
                const int callers = 16;
                using var attempting = new CountdownEvent(callers);
                var settings = new ProbeSettings(directory)
                {
                    DuringLoad = () => { loadEntered.Set(); allowLoad.Wait(); }
                };
                var loads = Enumerable.Range(0, callers).Select(_ => Task.Factory.StartNew(() =>
                {
                    start.Wait();
                    attempting.Signal();
                    return settings.Store;
                }, CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default)).ToArray();
                start.Set();
                bool reachedLoad;
                try
                {
                    reachedLoad = attempting.Wait(TimeSpan.FromSeconds(5)) && loadEntered.Wait(TimeSpan.FromSeconds(5));
                }
                finally { allowLoad.Set(); }
                var stores = await Task.WhenAll(loads);
                Check(reachedLoad, "Concurrent Store callers reach one intentionally gated production load");
                Check(settings.LoadCalls == 1, "Concurrent first Store access invokes LoadStore exactly once");
                Check(stores.All(store => ReferenceEquals(store, stores[0])), "Every concurrent Store caller receives the same cached instance");
                Check(stores.All(store => store.Revision == 77 && store.Name == "loaded fixture"),
                    "Concurrent Store initialization retains the actual persisted fixture values");
            }

            {
                var directory = DirectoryFor("serialized-save");
                var path = Path.Combine(directory, "settings.json");
                var settings = new ProbeSettings(directory);
                settings.SynchronizeStoreOrThrow();
                using var start = new ManualResetEventSlim();
                using var serializeEntered = new ManualResetEventSlim();
                using var allowSerialization = new ManualResetEventSlim();
                const int callers = 12;
                using var attempting = new CountdownEvent(callers);
                settings.DuringSerialization = () => { serializeEntered.Set(); allowSerialization.Wait(); };
                var saves = Enumerable.Range(0, callers).Select(index => Task.Factory.StartNew(() =>
                {
                    start.Wait();
                    attempting.Signal();
                    if (index % 3 == 0) settings.Save();
                    else if (index % 3 == 1) settings.SynchronizeStore();
                    else settings.SynchronizeStoreOrThrow();
                }, CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default)).ToArray();
                start.Set();
                bool reachedSerialization;
                try
                {
                    reachedSerialization = attempting.Wait(TimeSpan.FromSeconds(5)) && serializeEntered.Wait(TimeSpan.FromSeconds(5));
                }
                finally { allowSerialization.Set(); }
                await Task.WhenAll(saves);
                Check(reachedSerialization, "Concurrent Save callers reach an intentionally gated production serialization");
                Check(settings.MaximumActiveSerializations == 1, "All three production Save APIs serialize a shared Store one caller at a time");
                Check(settings.SerializationCalls == callers + 1, "Every concurrent Save request serializes and persists once");
                Check((int?)JObject.Parse(AtomicSettingsFile.ReadSnapshot(path))["Revision"] == callers + 1
                    && (int?)JObject.Parse(AtomicSettingsFile.ReadSnapshot(path + ".previous"))["Revision"] == callers
                    && NoPending(directory), "Serialized Save publishes the final revision with its immediate predecessor and no pending files");
            }

            Console.WriteLine($"PASS: {checks} atomic settings persistence/concurrency checks (temporary JSON only; no user AppData or hardware I/O)");
            return checks;
        }
        finally
        {
            var resolved = Path.GetFullPath(root);
            var temporaryParent = Path.GetFullPath(Path.GetTempPath()).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            if (!resolved.StartsWith(temporaryParent, StringComparison.OrdinalIgnoreCase)
                || !Path.GetFileName(resolved).StartsWith("Y7000p-AtomicSettings-", StringComparison.Ordinal))
                throw new InvalidOperationException("Refusing to remove an atomic-settings fixture outside its temporary root.");
            Directory.Delete(resolved, recursive: true);
        }
    }

    private static string Payload(int writer, int sequence) => new string((char)('A' + writer), 16384) + " / 中文 fixture / " + sequence;

    private static string Document(int writer, int sequence) => JsonConvert.SerializeObject(new
    {
        Writer = writer,
        Sequence = sequence,
        Payload = Payload(writer, sequence)
    });

    private static bool ValidDocument(string json)
    {
        var document = JObject.Parse(json);
        return document["Writer"] is JValue { Type: JTokenType.Integer } && document["Sequence"] is JValue { Type: JTokenType.Integer }
            && (int?)document["Writer"] is >= 0 and < 4 && (int?)document["Sequence"] is >= 0 and <= 24
            && (string?)document["Payload"] == Payload((int)document["Writer"]!, (int)document["Sequence"]!);
    }

    private sealed class FixtureStore
    {
        public int Revision { get; set; }
        public string Name { get; set; } = "default fixture";
    }

    private sealed class FaultingStore
    {
        public int Revision { get; set; }
        [JsonIgnore] public bool FailSerialization { get; set; }
        public string Payload => FailSerialization ? throw new InvalidOperationException("fixture serializer failed") : "valid fixture";
    }

    private sealed class FaultingSettings(string directory) : AbstractSettings<FaultingStore>("settings.json", directory) { }

    private sealed class ReplacingOnDeserializeSettings : AbstractSettings<FixtureStore>
    {
        private readonly string _path;
        private readonly string _replacement;
        public string? ObservedSnapshot { get; private set; }

        public ReplacingOnDeserializeSettings(string directory, string replacement) : base("settings.json", directory)
        {
            _path = Path.Combine(directory, "settings.json");
            _replacement = replacement;
        }

        protected override FixtureStore? DeserializeStore(string json)
        {
            ObservedSnapshot = json;
            AtomicSettingsFile.Write(_path, _replacement);
            throw new JsonSerializationException("fixture target replaced after the original snapshot was read");
        }
    }

    private sealed class ProbeSettings(string directory) : AbstractSettings<FixtureStore>("settings.json", directory)
    {
        private int _loadCalls;
        private int _serializationCalls;
        private int _activeSerializations;
        private int _maximumActiveSerializations;
        public int LoadCalls => Volatile.Read(ref _loadCalls);
        public int SerializationCalls => Volatile.Read(ref _serializationCalls);
        public int MaximumActiveSerializations => Volatile.Read(ref _maximumActiveSerializations);
        public Action? DuringLoad { get; init; }
        public Action? DuringSerialization { get; set; }

        public override FixtureStore? LoadStore()
        {
            Interlocked.Increment(ref _loadCalls);
            DuringLoad?.Invoke();
            return base.LoadStore();
        }

        protected override string SerializeStore(FixtureStore store)
        {
            var active = Interlocked.Increment(ref _activeSerializations);
            var maximum = Volatile.Read(ref _maximumActiveSerializations);
            while (active > maximum)
            {
                var observed = Interlocked.CompareExchange(ref _maximumActiveSerializations, active, maximum);
                if (observed == maximum) break;
                maximum = observed;
            }
            try
            {
                DuringSerialization?.Invoke();
                store.Revision = Interlocked.Increment(ref _serializationCalls);
                return base.SerializeStore(store);
            }
            finally { Interlocked.Decrement(ref _activeSerializations); }
        }
    }
}
