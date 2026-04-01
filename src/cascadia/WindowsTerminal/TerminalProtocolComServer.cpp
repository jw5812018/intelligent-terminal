// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"

#include "TerminalProtocolComServer.h"
#include "WindowEmperor.h"
#include "AppHost.h"

#include <json/json.h>
#include <til/io.h>

using namespace Microsoft::WRL;

// Static state — set once before registration, never mutated.
WindowEmperor* TerminalProtocolComServer::s_emperor = nullptr;

static DWORD g_comRegistration = 0;
static std::shared_mutex g_mtx;

void TerminalProtocolComServer::s_setEmperor(WindowEmperor* emperor) noexcept
{
    s_emperor = emperor;
}

HRESULT TerminalProtocolComServer::s_StartListening()
try
{
    std::unique_lock lock{ g_mtx };

    const auto classFactory = Make<SimpleClassFactory<TerminalProtocolComServer>>();
    RETURN_LAST_ERROR_IF_NULL(classFactory);

    ComPtr<IUnknown> unk;
    RETURN_IF_FAILED(classFactory.As(&unk));

    RETURN_IF_FAILED(CoRegisterClassObject(
        __uuidof(TerminalProtocolComServer),
        unk.Get(),
        CLSCTX_LOCAL_SERVER,
        REGCLS_MULTIPLEUSE,
        &g_comRegistration));

    return S_OK;
}
CATCH_RETURN()

HRESULT TerminalProtocolComServer::s_StopListening()
{
    std::unique_lock lock{ g_mtx };

    if (g_comRegistration)
    {
        RETURN_IF_FAILED(CoRevokeClassObject(g_comRegistration));
        g_comRegistration = 0;
    }

    return S_OK;
}

// ============================================================================
// Helper: get TerminalPage from AppHost
// ============================================================================

static winrt::TerminalApp::TerminalPage _getPage(AppHost* host)
{
    if (!host)
        return nullptr;
    const auto logic = host->Logic();
    if (!logic)
        return nullptr;
    const auto root = logic.GetRoot();
    if (!root)
        return nullptr;
    return root.try_as<winrt::TerminalApp::TerminalPage>();
}

// Helper: parse a JSON string into Json::Value
static bool _parseJson(const std::string& str, Json::Value& out)
{
    Json::CharReaderBuilder rb;
    std::string errs;
    std::istringstream ss(str);
    return Json::parseFromStream(rb, ss, &out, &errs);
}

// ============================================================================
// Meta
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::Authenticate(BSTR token, BOOL* authenticated, BSTR* protocolVersion)
try
{
    RETURN_HR_IF_NULL(E_POINTER, authenticated);
    RETURN_HR_IF_NULL(E_POINTER, protocolVersion);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    *authenticated = FALSE;
    *protocolVersion = nullptr;

    const auto tokenStr = token ? winrt::to_string(std::wstring_view(token, SysStringLen(token))) : std::string{};
    const auto& expectedToken = s_emperor->GetMcpToken();

    // DEV BYPASS: allow empty token to authenticate without credentials.
    // TODO: Remove this bypass before shipping.
    if (tokenStr.empty())
    {
        _authenticated = true;
    }
    else
    {
        // Constant-time comparison to prevent timing attacks.
        bool match = (tokenStr.size() == expectedToken.size());
        volatile bool dummy = false;
        for (size_t i = 0; i < std::min(tokenStr.size(), expectedToken.size()); ++i)
        {
            if (tokenStr[i] != expectedToken[i])
                dummy = true;
        }
        _authenticated = match && !dummy;
    }

    *authenticated = _authenticated ? TRUE : FALSE;
    *protocolVersion = SysAllocString(L"1.0");
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::GetCapabilities(BSTR* protocolVersion, BSTR* supportedMethodsJson)
try
{
    RETURN_HR_IF_NULL(E_POINTER, protocolVersion);
    RETURN_HR_IF_NULL(E_POINTER, supportedMethodsJson);

    *protocolVersion = SysAllocString(L"1.0");

    static const std::vector<std::string> supportedMethods = {
        "authenticate",
        "get_capabilities",
        "get_active_pane",
        "list_windows",
        "list_tabs",
        "list_panes",
        "read_pane_output",
        "get_process_status",
        "get_session_variable",
        "get_settings",
        "create_tab",
        "split_pane",
        "close_pane",
        "send_input",
        "set_session_variable",
        "set_settings",
    };

    Json::Value methods(Json::arrayValue);
    for (const auto& m : supportedMethods)
        methods.append(m);

    Json::StreamWriterBuilder wb;
    wb["indentation"] = "";
    *supportedMethodsJson = SysAllocString(winrt::to_hstring(Json::writeString(wb, methods)).c_str());
    return S_OK;
}
CATCH_RETURN()

// ============================================================================
// Queries
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::GetActivePane(PROTOCOL_PANE_INFO* result)
try
{
    RETURN_HR_IF_NULL(E_POINTER, result);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    memset(result, 0, sizeof(*result));

    const auto host = s_emperor->GetMostRecentWindow();
    RETURN_HR_IF_NULL(E_FAIL, host);

    const auto page = _getPage(host);
    RETURN_HR_IF_NULL(E_FAIL, page);

    const auto info = page.GetProtocolActivePane();
    if (info.PaneId.empty())
        return E_FAIL;

    const auto& props = host->Logic().WindowProperties();

    result->PaneId = SysAllocString(info.PaneId.c_str());
    result->TabId = SysAllocString(info.TabId.c_str());
    result->WindowId = SysAllocString(winrt::to_hstring(std::to_string(props.WindowId())).c_str());
    result->Title = SysAllocString(info.Title.c_str());
    result->Profile = SysAllocString(info.Profile.c_str());
    result->IsActive = info.IsActive ? TRUE : FALSE;
    result->Pid = info.Pid;
    result->Rows = info.Rows;
    result->Columns = info.Columns;
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::ListWindows(UINT32* count, PROTOCOL_WINDOW_INFO** results)
try
{
    RETURN_HR_IF_NULL(E_POINTER, count);
    RETURN_HR_IF_NULL(E_POINTER, results);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    *count = 0;
    *results = nullptr;

    const auto mostRecent = s_emperor->GetMostRecentWindow();

    // Count windows first.
    std::vector<PROTOCOL_WINDOW_INFO> items;
    auto cleanupItems = wil::scope_exit([&]() {
        for (auto& i : items) { SysFreeString(i.WindowId); SysFreeString(i.Title); }
    });

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto logic = host->Logic();
        if (!logic)
            continue;

        const auto& props = logic.WindowProperties();
        PROTOCOL_WINDOW_INFO info{};
        info.WindowId = SysAllocString(winrt::to_hstring(std::to_string(props.WindowId())).c_str());
        info.Title = SysAllocString(props.WindowNameForDisplay().c_str());
        info.IsFocused = (host.get() == mostRecent) ? TRUE : FALSE;
        info.TabCount = logic.TabCount();
        items.push_back(info);
    }

    if (items.empty())
        return S_OK;

    *count = static_cast<UINT32>(items.size());
    *results = static_cast<PROTOCOL_WINDOW_INFO*>(CoTaskMemAlloc(items.size() * sizeof(PROTOCOL_WINDOW_INFO)));
    RETURN_HR_IF_NULL(E_OUTOFMEMORY, *results);
    memcpy(*results, items.data(), items.size() * sizeof(PROTOCOL_WINDOW_INFO));
    cleanupItems.release();
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::ListTabs(BSTR windowIdFilter, UINT32* count, PROTOCOL_TAB_INFO** results)
try
{
    RETURN_HR_IF_NULL(E_POINTER, count);
    RETURN_HR_IF_NULL(E_POINTER, results);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    *count = 0;
    *results = nullptr;

    const auto filter = windowIdFilter ? winrt::to_string(std::wstring_view(windowIdFilter, SysStringLen(windowIdFilter))) : std::string{};

    std::vector<PROTOCOL_TAB_INFO> items;
    auto cleanupItems = wil::scope_exit([&]() {
        for (auto& i : items) { SysFreeString(i.TabId); SysFreeString(i.WindowId); SysFreeString(i.Title); }
    });

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto logic = host->Logic();
        if (!logic)
            continue;

        const auto& props = logic.WindowProperties();
        const auto windowIdStr = std::to_string(props.WindowId());
        if (!filter.empty() && windowIdStr != filter)
            continue;

        const auto page = _getPage(host.get());
        if (!page)
            continue;

        const auto tabs = page.GetProtocolTabs();
        for (uint32_t i = 0; i < tabs.Size(); ++i)
        {
            const auto& t = tabs.GetAt(i);
            PROTOCOL_TAB_INFO info{};
            info.TabId = SysAllocString(t.TabId.c_str());
            info.WindowId = SysAllocString(winrt::to_hstring(windowIdStr).c_str());
            info.Title = SysAllocString(t.Title.c_str());
            info.IsActive = t.IsActive ? TRUE : FALSE;
            info.PaneCount = t.PaneCount;
            items.push_back(info);
        }
    }

    if (items.empty())
        return S_OK;

    *count = static_cast<UINT32>(items.size());
    *results = static_cast<PROTOCOL_TAB_INFO*>(CoTaskMemAlloc(items.size() * sizeof(PROTOCOL_TAB_INFO)));
    RETURN_HR_IF_NULL(E_OUTOFMEMORY, *results);
    memcpy(*results, items.data(), items.size() * sizeof(PROTOCOL_TAB_INFO));
    cleanupItems.release();
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::ListPanes(BSTR windowIdFilter, BSTR tabIdFilter, UINT32* count, PROTOCOL_PANE_INFO** results)
try
{
    RETURN_HR_IF_NULL(E_POINTER, count);
    RETURN_HR_IF_NULL(E_POINTER, results);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    *count = 0;
    *results = nullptr;

    const auto winFilter = windowIdFilter ? winrt::to_string(std::wstring_view(windowIdFilter, SysStringLen(windowIdFilter))) : std::string{};
    const auto tabFilter = tabIdFilter ? winrt::to_string(std::wstring_view(tabIdFilter, SysStringLen(tabIdFilter))) : std::string{};

    std::vector<PROTOCOL_PANE_INFO> items;
    auto cleanupItems = wil::scope_exit([&]() {
        for (auto& i : items)
        {
            SysFreeString(i.PaneId); SysFreeString(i.TabId); SysFreeString(i.WindowId);
            SysFreeString(i.Title); SysFreeString(i.Profile);
        }
    });

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto logic = host->Logic();
        if (!logic)
            continue;

        const auto& props = logic.WindowProperties();
        const auto windowIdStr = std::to_string(props.WindowId());
        if (!winFilter.empty() && windowIdStr != winFilter)
            continue;

        const auto page = _getPage(host.get());
        if (!page)
            continue;

        const auto panes = page.GetProtocolPanes(winrt::to_hstring(tabFilter));
        for (uint32_t i = 0; i < panes.Size(); ++i)
        {
            const auto& p = panes.GetAt(i);
            PROTOCOL_PANE_INFO info{};
            info.PaneId = SysAllocString(p.PaneId.c_str());
            info.TabId = SysAllocString(p.TabId.c_str());
            info.WindowId = SysAllocString(winrt::to_hstring(windowIdStr).c_str());
            info.Title = SysAllocString(p.Title.c_str());
            info.Profile = SysAllocString(p.Profile.c_str());
            info.IsActive = p.IsActive ? TRUE : FALSE;
            info.Pid = p.Pid;
            info.Rows = p.Rows;
            info.Columns = p.Columns;
            items.push_back(info);
        }
    }

    if (items.empty())
        return S_OK;

    *count = static_cast<UINT32>(items.size());
    *results = static_cast<PROTOCOL_PANE_INFO*>(CoTaskMemAlloc(items.size() * sizeof(PROTOCOL_PANE_INFO)));
    RETURN_HR_IF_NULL(E_OUTOFMEMORY, *results);
    memcpy(*results, items.data(), items.size() * sizeof(PROTOCOL_PANE_INFO));
    cleanupItems.release();
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::ReadPaneOutput(BSTR paneId, BSTR source, INT32 maxLines, PROTOCOL_PANE_OUTPUT* result)
try
{
    RETURN_HR_IF_NULL(E_POINTER, result);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    memset(result, 0, sizeof(*result));

    const auto paneIdStr = paneId ? winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId))) : std::string{};
    const auto sourceStr = source ? winrt::to_string(std::wstring_view(source, SysStringLen(source))) : std::string("scrollback");

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        const auto info = page.ReadProtocolPaneOutput(
            winrt::to_hstring(paneIdStr), winrt::to_hstring(sourceStr), maxLines);
        if (info.PaneId.empty())
            continue;

        result->PaneId = SysAllocString(info.PaneId.c_str());
        result->Content = SysAllocString(info.Content.c_str());
        result->LineCount = info.LineCount;
        result->Truncated = info.Truncated ? TRUE : FALSE;
        return S_OK;
    }

    return E_FAIL; // Pane not found
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::GetProcessStatus(BSTR paneId, PROTOCOL_PROCESS_STATUS* result)
try
{
    RETURN_HR_IF_NULL(E_POINTER, result);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    memset(result, 0, sizeof(*result));

    const auto paneIdStr = paneId ? winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId))) : std::string{};

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        const auto info = page.GetProtocolProcessStatus(winrt::to_hstring(paneIdStr));
        if (info.PaneId.empty())
            continue;

        result->PaneId = SysAllocString(info.PaneId.c_str());
        result->State = SysAllocString(info.State.c_str());
        result->Pid = info.Pid;
        result->ExitCode = info.ExitCode;
        result->HasExitCode = info.HasExitCode ? TRUE : FALSE;
        return S_OK;
    }

    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::GetSessionVariable(BSTR paneId, BSTR name, PROTOCOL_SESSION_VARIABLE* result)
try
{
    RETURN_HR_IF_NULL(E_POINTER, result);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    memset(result, 0, sizeof(*result));

    const auto paneIdStr = paneId ? winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId))) : std::string{};
    const auto nameStr = name ? winrt::to_string(std::wstring_view(name, SysStringLen(name))) : std::string{};

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        const auto info = page.GetProtocolSessionVariable(
            winrt::to_hstring(paneIdStr), winrt::to_hstring(nameStr));
        if (info.PaneId.empty())
            continue;

        result->PaneId = SysAllocString(info.PaneId.c_str());
        result->Name = SysAllocString(info.Name.c_str());
        result->Value = SysAllocString(info.Value.c_str());
        result->Exists = info.Exists ? TRUE : FALSE;
        return S_OK;
    }

    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::GetSettings(BSTR* settingsJson)
try
{
    RETURN_HR_IF_NULL(E_POINTER, settingsJson);
    *settingsJson = nullptr;

    const std::filesystem::path settingsPath{ std::wstring_view{ winrt::Microsoft::Terminal::Settings::Model::CascadiaSettings::SettingsPath() } };
    const auto content = til::io::read_file_as_utf8_string_if_exists(settingsPath);

    *settingsJson = SysAllocString(winrt::to_hstring(content).c_str());
    return S_OK;
}
CATCH_RETURN()

// ============================================================================
// Mutations
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::CreateTab(BSTR windowId, BSTR profile, BSTR commandline,
                                                   BSTR title, BOOL suppressAppTitle,
                                                   BOOL injectMcpCredentials, BOOL background,
                                                   PROTOCOL_TAB_CREATION_RESULT* result)
try
{
    RETURN_HR_IF_NULL(E_POINTER, result);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    memset(result, 0, sizeof(*result));

    // Find target window.
    AppHost* targetHost = nullptr;
    if (windowId && SysStringLen(windowId) > 0)
    {
        const auto windowIdStr = winrt::to_string(std::wstring_view(windowId, SysStringLen(windowId)));
        targetHost = s_emperor->GetWindowById(std::stoull(windowIdStr));
    }
    else
    {
        targetHost = s_emperor->GetMostRecentWindow();
    }
    RETURN_HR_IF_NULL(E_FAIL, targetHost);

    const auto page = _getPage(targetHost);
    RETURN_HR_IF_NULL(E_FAIL, page);

    // Build NewTerminalArgs.
    winrt::Microsoft::Terminal::Settings::Model::NewTerminalArgs newTermArgs;
    if (profile && SysStringLen(profile) > 0)
        newTermArgs.Profile(winrt::hstring(std::wstring_view(profile, SysStringLen(profile))));
    if (commandline && SysStringLen(commandline) > 0)
        newTermArgs.Commandline(winrt::hstring(std::wstring_view(commandline, SysStringLen(commandline))));
    if (title && SysStringLen(title) > 0)
    {
        newTermArgs.TabTitle(winrt::hstring(std::wstring_view(title, SysStringLen(title))));
        if (suppressAppTitle)
            newTermArgs.SuppressApplicationTitle(true);
    }

    // Inject MCP credentials when requested.
    if (injectMcpCredentials)
    {
        const auto& token = s_emperor->GetMcpToken();
        if (!token.empty())
        {
            page.SetPendingProtocolEnv(L"WT_MCP_TOKEN", winrt::to_hstring(token));
            page.SetPendingProtocolEnv(L"WT_PIPE_NAME", winrt::hstring{ s_emperor->GetProtocolPipeName() });
            const auto& comClsid = s_emperor->GetComClsid();
            if (!comClsid.empty())
                page.SetPendingProtocolEnv(L"WT_COM_CLSID", winrt::hstring{ comClsid });
        }
    }

    const auto cr = page.CreateProtocolTab(newTermArgs, background ? true : false);
    if (cr.TabId.empty())
        return E_FAIL;

    const auto& props = targetHost->Logic().WindowProperties();
    result->TabId = SysAllocString(cr.TabId.c_str());
    result->PaneId = SysAllocString(cr.PaneId.c_str());
    result->WindowId = SysAllocString(winrt::to_hstring(std::to_string(props.WindowId())).c_str());
    result->Pid = cr.Pid;
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SplitPane(BSTR paneId, BSTR direction, float size,
                                                    BSTR profile, BSTR commandline,
                                                    BOOL injectMcpCredentials, BOOL background,
                                                    PROTOCOL_TAB_CREATION_RESULT* result)
try
{
    RETURN_HR_IF_NULL(E_POINTER, result);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    RETURN_HR_IF_NULL(E_INVALIDARG, paneId);
    memset(result, 0, sizeof(*result));

    const auto paneIdHstr = winrt::hstring(std::wstring_view(paneId, SysStringLen(paneId)));

    // Map direction string to SplitDirection enum.
    auto splitDir = winrt::Microsoft::Terminal::Settings::Model::SplitDirection::Right;
    if (direction && SysStringLen(direction) > 0)
    {
        const auto dirStr = winrt::to_string(std::wstring_view(direction, SysStringLen(direction)));
        if (dirStr == "left")
            splitDir = winrt::Microsoft::Terminal::Settings::Model::SplitDirection::Left;
        else if (dirStr == "up")
            splitDir = winrt::Microsoft::Terminal::Settings::Model::SplitDirection::Up;
        else if (dirStr == "down")
            splitDir = winrt::Microsoft::Terminal::Settings::Model::SplitDirection::Down;
    }

    // Build NewTerminalArgs.
    winrt::Microsoft::Terminal::Settings::Model::NewTerminalArgs newTermArgs;
    if (profile && SysStringLen(profile) > 0)
        newTermArgs.Profile(winrt::hstring(std::wstring_view(profile, SysStringLen(profile))));
    if (commandline && SysStringLen(commandline) > 0)
        newTermArgs.Commandline(winrt::hstring(std::wstring_view(commandline, SysStringLen(commandline))));

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        // Inject MCP credentials when requested.
        if (injectMcpCredentials)
        {
            const auto& token = s_emperor->GetMcpToken();
            if (!token.empty())
            {
                page.SetPendingProtocolEnv(L"WT_MCP_TOKEN", winrt::to_hstring(token));
                page.SetPendingProtocolEnv(L"WT_PIPE_NAME", winrt::hstring{ s_emperor->GetProtocolPipeName() });
                const auto& comClsid = s_emperor->GetComClsid();
                if (!comClsid.empty())
                    page.SetPendingProtocolEnv(L"WT_COM_CLSID", winrt::hstring{ comClsid });
            }
        }

        const auto cr = page.SplitProtocolPane(paneIdHstr, splitDir, size, newTermArgs, background ? true : false);
        if (cr.TabId.empty())
            continue; // pane not in this window

        const auto& props = host->Logic().WindowProperties();
        result->TabId = SysAllocString(cr.TabId.c_str());
        result->PaneId = SysAllocString(cr.PaneId.c_str());
        result->WindowId = SysAllocString(winrt::to_hstring(std::to_string(props.WindowId())).c_str());
        result->Pid = cr.Pid;
        return S_OK;
    }

    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::ClosePane(BSTR paneId)
try
{
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    RETURN_HR_IF_NULL(E_INVALIDARG, paneId);

    const auto paneIdHstr = winrt::hstring(std::wstring_view(paneId, SysStringLen(paneId)));

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        if (page.CloseProtocolPane(paneIdHstr))
            return S_OK;
    }

    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SendInput(BSTR paneId, BSTR text)
try
{
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    RETURN_HR_IF_NULL(E_INVALIDARG, paneId);
    RETURN_HR_IF_NULL(E_INVALIDARG, text);

    const auto paneIdHstr = winrt::hstring(std::wstring_view(paneId, SysStringLen(paneId)));
    const auto textHstr = winrt::hstring(std::wstring_view(text, SysStringLen(text)));

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        if (page.SendProtocolInput(paneIdHstr, textHstr))
            return S_OK;
    }

    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SetSessionVariable(BSTR paneId, BSTR name, BSTR value)
try
{
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_emperor);
    RETURN_HR_IF_NULL(E_INVALIDARG, paneId);
    RETURN_HR_IF_NULL(E_INVALIDARG, name);

    const auto paneIdHstr = winrt::hstring(std::wstring_view(paneId, SysStringLen(paneId)));
    const auto nameHstr = winrt::hstring(std::wstring_view(name, SysStringLen(name)));
    const auto valueHstr = (value && SysStringLen(value) > 0)
        ? winrt::hstring(std::wstring_view(value, SysStringLen(value)))
        : winrt::hstring{};

    for (const auto& host : s_emperor->GetWindows())
    {
        const auto page = _getPage(host.get());
        if (!page)
            continue;

        if (page.SetProtocolSessionVariable(paneIdHstr, nameHstr, valueHstr))
            return S_OK;
    }

    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SetSettings(BSTR settingsContent, BSTR* backupPath)
try
{
    RETURN_HR_IF_NULL(E_POINTER, backupPath);
    RETURN_HR_IF_NULL(E_INVALIDARG, settingsContent);
    *backupPath = nullptr;

    const auto contentStr = winrt::to_string(std::wstring_view(settingsContent, SysStringLen(settingsContent)));
    if (contentStr.empty())
        return E_INVALIDARG;

    // Validate that it's valid JSON.
    Json::Value parsedSettings;
    if (!_parseJson(contentStr, parsedSettings))
        return E_INVALIDARG;

    // Get the settings path and create a backup.
    const std::filesystem::path settingsPath{ std::wstring_view{ winrt::Microsoft::Terminal::Settings::Model::CascadiaSettings::SettingsPath() } };
    const auto settingsDir = settingsPath.parent_path();

    // Create timestamped backup.
    const auto now = std::chrono::system_clock::now();
    const auto time = std::chrono::system_clock::to_time_t(now);
    std::tm tm{};
    localtime_s(&tm, &time);

    wchar_t timeStr[64];
    wcsftime(timeStr, std::size(timeStr), L"%Y-%m-%dT%H-%M-%S", &tm);

    const auto backup = settingsDir / fmt::format(L"settings.backup.{}.json", timeStr);

    // Copy current settings to backup.
    std::error_code ec;
    std::filesystem::copy_file(settingsPath, backup, std::filesystem::copy_options::overwrite_existing, ec);

    // Clean up old backups — keep only the most recent 5.
    std::vector<std::filesystem::path> backups;
    for (const auto& entry : std::filesystem::directory_iterator(settingsDir, ec))
    {
        if (entry.is_regular_file() && entry.path().filename().wstring().starts_with(L"settings.backup."))
            backups.push_back(entry.path());
    }
    if (backups.size() > 5)
    {
        std::sort(backups.begin(), backups.end());
        for (size_t i = 0; i < backups.size() - 5; ++i)
            std::filesystem::remove(backups[i], ec);
    }

    // Write the new settings.
    til::io::write_utf8_string_to_file_atomic(settingsPath, contentStr);

    *backupPath = SysAllocString(backup.wstring().c_str());
    return S_OK;
}
CATCH_RETURN()
