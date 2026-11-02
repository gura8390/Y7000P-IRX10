using System;
using System.Threading;
using System.Windows;
using System.Windows.Controls;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.System;
using LenovoLegionToolkit.WPF.Resources;
using LenovoLegionToolkit.WPF.Settings;

namespace LenovoLegionToolkit.WPF.Windows.Settings;

public partial class SensorSettingsWindow
{
    private readonly HardwareSensorSettings _sensorsSettings = IoCContainer.Resolve<HardwareSensorSettings>();
    private readonly ApplicationSettings _appSettings = IoCContainer.Resolve<ApplicationSettings>();
    private readonly CpuThermalController _thermalController = IoCContainer.Resolve<CpuThermalController>();
    private readonly Func<bool> _hasMultipleGpus;

    public SensorSettingsWindow() : this(Displays.HasMultipleGpus)
    {
    }

    internal SensorSettingsWindow(Func<bool> hasMultipleGpus)
    {
        _hasMultipleGpus = hasMultipleGpus ?? throw new ArgumentNullException(nameof(hasMultipleGpus));
        InitializeComponent();
        _thermalShadowCheckBox.IsChecked = _appSettings.Store.EnableThermalShadow;
        _cpuThermalCheckBox.IsChecked = _appSettings.Store.EnableCpuThermalControl;
        _cpuThermalStatus.Text = _thermalController.Status;
        _thermalController.StatusChanged += OnThermalStatusChanged;
        Closed += (_, _) => _thermalController.StatusChanged -= OnThermalStatusChanged;

        _cpuFrequencySelector.SelectedIndex = _sensorsSettings.Store.ShowCpuAverageFrequency ? 1 : 0;
        _memoryDisplayModeSelector.SelectedIndex = _sensorsSettings.Store.DisplayMemoryInGigabytes ? 1 : 0;

        var controller = IoCContainer.Resolve<SensorsGroupController>();
        int coreCount = controller.AvailableVoltageCoreCount;
        if (coreCount == 0) coreCount = 1;
        for (int i = 0; i < coreCount; i++)
        {
            _voltageCoreSelector.Items.Add(new ComboBoxItem { Content = $"{Resource.Core} {i + 1}" });
        }

        _voltageCoreSelector.SelectedIndex = Math.Clamp(_sensorsSettings.Store.CpuVoltageCoreIndex, 0, coreCount - 1);

        _voltageSelector.SelectedIndex = (int)_sensorsSettings.Store.CpuVoltageMode;
        UpdateCoreSelectorVisibility();

        _cpuTemperatureSourceSelector.SelectedIndex = (int)_sensorsSettings.Store.CpuTemperatureSource;

        if (_hasMultipleGpus())
        {
            _gpuSelectionCard.Visibility = Visibility.Visible;
            _gpuSelector.SelectedIndex = _sensorsSettings.Store.SelectedGpuIsIgpu ? 1 : 0;
        }
        else
        {
            _gpuSelectionCard.Visibility = Visibility.Collapsed;
        }

        _temperatureUnitSelector.SelectedIndex = _appSettings.Store.TemperatureUnit == TemperatureUnit.F ? 1 : 0;
    }

    private void _voltageSelector_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        UpdateCoreSelectorVisibility();
    }

    private void UpdateCoreSelectorVisibility()
    {
        _voltageCoreSelector.Visibility = _voltageSelector.SelectedIndex == (int)CpuVoltageMode.Core
            ? Visibility.Visible : Visibility.Collapsed;
    }

    private async void DefaultButton_Click(object sender, RoutedEventArgs e)
    {
        try { await _thermalController.StopAsync().WaitAsync(TimeSpan.FromSeconds(3)); }
        catch (Exception ex) { MessageBox.Show(this, ex.Message, "CPU 温控", MessageBoxButton.OK, MessageBoxImage.Error); return; }
        _sensorsSettings.Store.ShowCpuAverageFrequency = false;
        _sensorsSettings.Store.SelectedGpuIsIgpu = false;
        _sensorsSettings.Store.DisplayMemoryInGigabytes = false;
        _sensorsSettings.Store.CpuVoltageMode = CpuVoltageMode.Average;
        _sensorsSettings.Store.CpuVoltageCoreIndex = 0;
        _sensorsSettings.Store.CpuTemperatureSource = CpuTemperatureSource.Auto;
        _appSettings.Store.TemperatureUnit = TemperatureUnit.C;
        _appSettings.Store.EnableThermalShadow = false;
        IoCContainer.Resolve<SensorsGroupController>().ThermalShadowEnabled = false;
        _appSettings.Store.EnableCpuThermalControl = false;
        _cpuFrequencySelector.SelectedIndex = 0;
        _voltageSelector.SelectedIndex = 0;
        _voltageCoreSelector.SelectedIndex = 0;
        UpdateCoreSelectorVisibility();
        _cpuTemperatureSourceSelector.SelectedIndex = 0;
        _gpuSelector.SelectedIndex = 0;
        _memoryDisplayModeSelector.SelectedIndex = 0;
        _temperatureUnitSelector.SelectedIndex = 0;
        try
        {
            _appSettings.SynchronizeStoreOrThrow();
            _sensorsSettings.SynchronizeStoreOrThrow();
        }
        catch (Exception ex)
        {
            MessageBox.Show(this, $"默认设置未完整保存，重启后可能载入旧设置。当前温控状态：{_thermalController.Status}\n{ex.Message}",
                "设置保存失败", MessageBoxButton.OK, MessageBoxImage.Error);
            return;
        }

        Close();
    }

    private void CancelButton_Click(object sender, RoutedEventArgs e)
    {
        Close();
    }

    private void OnThermalStatusChanged(string status) => Dispatcher.BeginInvoke(() => _cpuThermalStatus.Text = status);

    private async void ApplyButton_Click(object sender, RoutedEventArgs e)
    {
        if (sender is System.Windows.Controls.Button button) button.IsEnabled = false;
        try
        {
            var enabled = _cpuThermalCheckBox.IsChecked == true;
            if (enabled)
            {
                if (App.IsInitializing)
                    throw new InvalidOperationException("启动配置和自动化动作仍在运行，请等初始化完成后再启用 CPU 温控。");
                using var startupCancellation = new CancellationTokenSource(TimeSpan.FromSeconds(10));
                await _thermalController.StartAsync(startupCancellation.Token).WaitAsync(startupCancellation.Token);
            }
            else await _thermalController.StopAsync().WaitAsync(TimeSpan.FromSeconds(3));
            _appSettings.Store.EnableCpuThermalControl = enabled;
        }
        catch (Exception ex)
        {
            MessageBox.Show(this, ex.Message, "CPU 温控未生效", MessageBoxButton.OK, MessageBoxImage.Error);
            if (sender is System.Windows.Controls.Button failedButton) failedButton.IsEnabled = true;
            return;
        }
        _appSettings.Store.EnableThermalShadow = _thermalShadowCheckBox.IsChecked == true;
        IoCContainer.Resolve<SensorsGroupController>().ThermalShadowEnabled = _appSettings.Store.EnableThermalShadow;
        _sensorsSettings.Store.ShowCpuAverageFrequency = _cpuFrequencySelector.SelectedIndex == 1;
        _sensorsSettings.Store.CpuVoltageMode = (CpuVoltageMode)_voltageSelector.SelectedIndex;
        _sensorsSettings.Store.CpuVoltageCoreIndex = _voltageCoreSelector.SelectedIndex;
        _sensorsSettings.Store.CpuTemperatureSource = (CpuTemperatureSource)_cpuTemperatureSourceSelector.SelectedIndex;
        if (_hasMultipleGpus())
        {
            _sensorsSettings.Store.SelectedGpuIsIgpu = _gpuSelector.SelectedIndex == 1;
        }

        _sensorsSettings.Store.DisplayMemoryInGigabytes = _memoryDisplayModeSelector.SelectedIndex == 1;
        _appSettings.Store.TemperatureUnit = _temperatureUnitSelector.SelectedIndex == 1 ? TemperatureUnit.F : TemperatureUnit.C;

        try
        {
            _appSettings.SynchronizeStoreOrThrow();
            _sensorsSettings.SynchronizeStoreOrThrow();
        }
        catch (Exception ex)
        {
            MessageBox.Show(this, $"设置未完整保存，重启后可能载入旧设置。当前温控状态：{_thermalController.Status}\n{ex.Message}",
                "设置保存失败", MessageBoxButton.OK, MessageBoxImage.Error);
            if (sender is System.Windows.Controls.Button failedButton) failedButton.IsEnabled = true;
            return;
        }
        Close();
    }
}
