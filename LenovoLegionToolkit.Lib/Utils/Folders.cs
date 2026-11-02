using System;
using System.IO;

namespace LenovoLegionToolkit.Lib.Utils;

public static class Folders
{
    // Isolated in-process fixtures set this before any settings/localization type
    // initializes. Normal application runs keep the default null path.
    internal static string? AppDataOverride = null;

    public static void EnsureParentDirectoryExists(string? filePath)
    {
        if (string.IsNullOrWhiteSpace(filePath))
        {
            return;
        }

        var folderPath = Path.GetDirectoryName(filePath);
        if (!string.IsNullOrEmpty(folderPath))
        {
            Directory.CreateDirectory(folderPath);
        }
    }

    public static string Program => AppDomain.CurrentDomain.SetupInformation.ApplicationBase ?? string.Empty;

    public static string AppData
    {
        get
        {
            if (AppDataOverride is not null)
            {
                Directory.CreateDirectory(AppDataOverride);
                return AppDataOverride;
            }

            var appData = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
            var folderPath = Path.Combine(appData, "LenovoLegionToolkit");
            Directory.CreateDirectory(folderPath);
            return folderPath;
        }
    }

    public static string Temp
    {
        get
        {
            var appData = Path.GetTempPath();
            var folderPath = Path.Combine(appData, "LenovoLegionToolkit");
            Directory.CreateDirectory(folderPath);
            return folderPath;
        }
    }
}
