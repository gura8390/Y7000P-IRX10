using System;
using System.IO;
using LenovoLegionToolkit.Lib.Utils;
using Newtonsoft.Json;
using Newtonsoft.Json.Converters;

namespace LenovoLegionToolkit.Lib.Settings;

public abstract class AbstractSettings<T> where T : class, new()
{
    protected readonly JsonSerializerSettings JsonSerializerSettings;
    private readonly string _settingsStorePath;
    private readonly string _settingsStoreDirectory;
    private readonly string _fileName;
    private T? _store;
    private readonly object _storeLock = new();

    protected virtual T Default => new();

    public T Store
    {
        get { lock (_storeLock) return _store ??= LoadStore() ?? Default; }
        protected set { lock (_storeLock) _store = value; }
    }

    protected AbstractSettings(string filename, string? storageDirectory = null)
    {
        JsonSerializerSettings = new()
        {
            Formatting = Formatting.Indented,
            TypeNameHandling = TypeNameHandling.Auto,
            ObjectCreationHandling = ObjectCreationHandling.Replace,
            Converters = { new StringEnumConverter() }
        };

        _fileName = filename;
        _settingsStoreDirectory = storageDirectory ?? Folders.AppData;
        _settingsStorePath = Path.Combine(_settingsStoreDirectory, _fileName);
    }

    public void Save()
    {
        try
        {
            SynchronizeStoreOrThrow();
        }
        catch (Exception ex)
        {
            Log.Instance.Trace($"Unable to save {_fileName}", ex);
        }
    }

    public void EnsureFileExists()
    {
        if (!File.Exists(_settingsStorePath))
        {
            Save();
        }
    }

    public void Reset()
    {
        lock (_storeLock) _store = null;
    }

    public virtual T? LoadStore()
    {
        T? store = null;
        string? settingsSerialized = null;
        try
        {
            settingsSerialized = AtomicSettingsFile.ReadSnapshot(_settingsStorePath);
            store = DeserializeStore(settingsSerialized);

            if (store is null)
                TryBackup(settingsSerialized);
        }
        catch
        {
            TryBackup(settingsSerialized);
        }

        return store;
    }

    public void SynchronizeStore()
    {
        try
        {
            SynchronizeStoreOrThrow();
        }
        catch (Exception ex)
        {
            Log.Instance.Trace($"Unable to synchronize {_fileName}", ex);
        }
    }

    // Critical UI actions need to distinguish a running in-memory change from a
    // successfully persisted setting. Legacy callers retain their logging behavior.
    public void SynchronizeStoreOrThrow()
    {
        lock (_storeLock)
        {
            var settingsSerialized = SerializeStore(Store);
            AtomicSettingsFile.Write(_settingsStorePath, settingsSerialized);
        }
    }

    protected virtual string SerializeStore(T store) => JsonConvert.SerializeObject(store, JsonSerializerSettings);

    protected virtual T? DeserializeStore(string json) => JsonConvert.DeserializeObject<T>(json, JsonSerializerSettings);

    private void TryBackup(string? failedSnapshot)
    {
        try
        {
            if (failedSnapshot is null && !File.Exists(_settingsStorePath))
                return;

            var backupFileName = $"{Path.GetFileNameWithoutExtension(_fileName)}_backup_{DateTime.UtcNow:yyyyMMddHHmmssfffffff}_{Guid.NewGuid():N}{Path.GetExtension(_fileName)}";
            var backupFilePath = Path.Combine(_settingsStoreDirectory, backupFileName);
            Folders.EnsureParentDirectoryExists(backupFilePath);
            if (failedSnapshot is not null)
                AtomicSettingsFile.Write(backupFilePath, failedSnapshot);
            else
                File.Copy(_settingsStorePath, backupFilePath);
        }
        catch (Exception ex)
        {
            Log.Instance.Trace($"Unable to create backup for {_fileName}", ex);
        }
    }
}
