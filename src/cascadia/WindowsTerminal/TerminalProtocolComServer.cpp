// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"

#include "TerminalProtocolComServer.h"
#include "ProtocolRequestHandler.h"
#include "WindowEmperor.h"
#include "AppHost.h"

#include <json/json.h>
#include <til/io.h>

#include <thread>

using namespace Microsoft::WRL;

// Static state — set once before registration, never mutated.
WindowEmperor* TerminalProtocolComServer::s_emperor = nullptr;
ProtocolRequestHandler* TerminalProtocolComServer::s_handler = nullptr;

static DWORD g_comRegistration = 0;
static std::shared_mutex g_mtx;
static std::thread g_comMtaThread;
static wil::unique_event g_comMtaStop;

// Static instance tracking for event delivery to COM clients
std::mutex TerminalProtocolComServer::s_instancesMutex;
std::vector<TerminalProtocolComServer*> TerminalProtocolComServer::s_instances;

void TerminalProtocolComServer::s_setEmperor(WindowEmperor* emperor) noexcept
{
    s_emperor = emperor;
}

void TerminalProtocolComServer::s_setHandler(ProtocolRequestHandler* handler) noexcept
{
    s_handler = handler;
}

HRESULT TerminalProtocolComServer::s_StartListening()
try
{
    std::unique_lock lock{ g_mtx };

    // Register the COM class factory on a dedicated MTA thread so that
    // incoming COM calls are dispatched to MTA worker threads rather than
    // the STA/UI thread.  This is critical for methods that block
    // (QuickPick waits for user input, PollEvents waits for events) —
    // dispatching those on the UI thread would deadlock or freeze the app.
    g_comMtaStop.create(wil::EventOptions::ManualReset);

    wil::unique_event ready(wil::EventOptions::ManualReset);
    HRESULT regHr = S_OK;

    g_comMtaThread = std::thread([&ready, &regHr]() {
        auto coInit = wil::CoInitializeEx(COINIT_MULTITHREADED);

        const auto classFactory = Make<SimpleClassFactory<TerminalProtocolComServer>>();
        if (!classFactory)
        {
            regHr = HRESULT_FROM_WIN32(GetLastError());
            ready.SetEvent();
            return;
        }

        ComPtr<IUnknown> unk;
        regHr = classFactory.As(&unk);
        if (FAILED(regHr))
        {
            ready.SetEvent();
            return;
        }

        regHr = CoRegisterClassObject(
            __uuidof(TerminalProtocolComServer),
            unk.Get(),
            CLSCTX_LOCAL_SERVER,
            REGCLS_MULTIPLEUSE,
            &g_comRegistration);

        ready.SetEvent();

        // Keep this MTA thread alive so the COM registration stays active.
        WaitForSingleObject(g_comMtaStop.get(), INFINITE);
    });

    ready.wait();
    RETURN_IF_FAILED(regHr);
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

    // Signal the MTA thread to exit
    if (g_comMtaStop)
    {
        g_comMtaStop.SetEvent();
    }
    if (g_comMtaThread.joinable())
    {
        g_comMtaThread.join();
    }

    return S_OK;
}

void TerminalProtocolComServer::_registerForEvents()
{
    // Initialize the event signal for PollEvents blocking
    _eventSignal.create(wil::EventOptions::ManualReset);

    std::lock_guard lock{ s_instancesMutex };
    s_instances.push_back(this);
}

void TerminalProtocolComServer::_unregisterFromEvents()
{
    std::lock_guard lock{ s_instancesMutex };
    std::erase(s_instances, this);
}

void TerminalProtocolComServer::s_BroadcastEventToComClients(const std::string& eventJson)
{
    std::lock_guard lock{ s_instancesMutex };
    for (auto* instance : s_instances)
    {
        if (!instance->_authenticated)
            continue;

        {
            std::lock_guard eLock{ instance->_eventMutex };
            // Cap queue to prevent unbounded memory growth
            if (instance->_eventQueue.size() < 1000)
            {
                instance->_eventQueue.push_back(eventJson);
            }
        }
        // Signal the event to wake up any blocking PollEvents call
        if (instance->_eventSignal)
        {
            instance->_eventSignal.SetEvent();
        }
    }
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
// JSON fallback — delegates to ProtocolRequestHandler
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::HandleRequest(BSTR requestJson, BSTR* responseJson)
try
{
    RETURN_HR_IF_NULL(E_POINTER, responseJson);
    *responseJson = nullptr;
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);
    RETURN_HR_IF_NULL(E_INVALIDARG, requestJson);

    const auto reqWide = std::wstring_view(requestJson, SysStringLen(requestJson));
    const auto reqUtf8 = winrt::to_string(reqWide);

    Json::Value request;
    if (!_parseJson(reqUtf8, request))
    {
        Json::Value errResp;
        errResp["type"] = "response";
        errResp["id"] = "";
        errResp["result"] = Json::nullValue;
        Json::Value err;
        err["code"] = "parse_error";
        err["message"] = "Failed to parse request JSON.";
        errResp["error"] = err;

        Json::StreamWriterBuilder wb;
        wb["indentation"] = "";
        *responseJson = SysAllocString(winrt::to_hstring(Json::writeString(wb, errResp)).c_str());
        return S_OK;
    }

    const auto response = s_handler->HandleRequest(request, _authenticated);

    Json::StreamWriterBuilder wb;
    wb["indentation"] = "";
    *responseJson = SysAllocString(winrt::to_hstring(Json::writeString(wb, response)).c_str());
    return S_OK;
}
CATCH_RETURN()

// ============================================================================
// Meta
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::Authenticate(BSTR token, BOOL* authenticated, BSTR* protocolVersion)
try
{
    RETURN_HR_IF_NULL(E_POINTER, authenticated);
    RETURN_HR_IF_NULL(E_POINTER, protocolVersion);
    *authenticated = FALSE;
    *protocolVersion = nullptr;

    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);

    const auto tokenStr = token ? winrt::to_string(std::wstring_view(token, SysStringLen(token))) : std::string{};

    // Build a JSON request and delegate to the existing handler.
    Json::Value params;
    params["token"] = tokenStr;
    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-auth";
    request["method"] = "authenticate";
    request["params"] = params;

    s_handler->HandleRequest(request, _authenticated);

    // Register for event delivery on successful authentication
    if (_authenticated)
    {
        _registerForEvents();
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

    // Build JSON array of method names from the canonical list in ProtocolRequestHandler.
    Json::Value methods(Json::arrayValue);
    for (const auto& m : ProtocolRequestHandler::GetSupportedMethods())
    {
        methods.append(m);
    }
    // Add COM-only methods not in the JSON handler's list
    methods.append("poll_events");

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

    const auto jsonStr = winrt::to_string(page.GetProtocolActivePaneJson());
    if (jsonStr.empty())
        return E_FAIL;

    Json::Value v;
    if (!_parseJson(jsonStr, v))
        return E_FAIL;

    const auto& props = host->Logic().WindowProperties();
    const auto windowId = std::to_string(props.WindowId());

    result->PaneId = SysAllocString(winrt::to_hstring(v.get("pane_id", "").asString()).c_str());
    result->TabId = SysAllocString(winrt::to_hstring(v.get("tab_id", "").asString()).c_str());
    result->WindowId = SysAllocString(winrt::to_hstring(windowId).c_str());
    result->Title = SysAllocString(winrt::to_hstring(v.get("title", "").asString()).c_str());
    result->Profile = SysAllocString(winrt::to_hstring(v.get("profile", "").asString()).c_str());
    result->IsActive = TRUE;
    result->Pid = v.get("pid", 0u).asUInt();
    result->Rows = 0;
    result->Columns = 0;
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

        const auto tabsJson = winrt::to_string(page.GetProtocolTabsJson());
        Json::Value tabs;
        if (!_parseJson(tabsJson, tabs) || !tabs.isArray())
            continue;

        for (const auto& t : tabs)
        {
            PROTOCOL_TAB_INFO info{};
            info.TabId = SysAllocString(winrt::to_hstring(t.get("tab_id", "").asString()).c_str());
            info.WindowId = SysAllocString(winrt::to_hstring(windowIdStr).c_str());
            info.Title = SysAllocString(winrt::to_hstring(t.get("title", "").asString()).c_str());
            info.IsActive = t.get("is_active", false).asBool() ? TRUE : FALSE;
            info.PaneCount = t.get("pane_count", 0u).asUInt();
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

        const auto panesJson = winrt::to_string(page.GetProtocolPanesJson(winrt::to_hstring(tabFilter)));
        Json::Value panes;
        if (!_parseJson(panesJson, panes) || !panes.isArray())
            continue;

        for (const auto& p : panes)
        {
            PROTOCOL_PANE_INFO info{};
            info.PaneId = SysAllocString(winrt::to_hstring(p.get("pane_id", "").asString()).c_str());
            info.TabId = SysAllocString(winrt::to_hstring(p.get("tab_id", "").asString()).c_str());
            info.WindowId = SysAllocString(winrt::to_hstring(windowIdStr).c_str());
            info.Title = SysAllocString(winrt::to_hstring(p.get("title", "").asString()).c_str());
            info.Profile = SysAllocString(winrt::to_hstring(p.get("profile", "").asString()).c_str());
            info.IsActive = p.get("is_active", false).asBool() ? TRUE : FALSE;
            info.Pid = p.get("pid", 0u).asUInt();
            info.Rows = p.isMember("size") ? p["size"].get("rows", 0).asInt() : 0;
            info.Columns = p.isMember("size") ? p["size"].get("columns", 0).asInt() : 0;
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

        const auto jsonStr = winrt::to_string(page.ReadProtocolPaneOutput(
            winrt::to_hstring(paneIdStr), winrt::to_hstring(sourceStr), maxLines));
        if (jsonStr.empty())
            continue;

        Json::Value v;
        if (!_parseJson(jsonStr, v))
            continue;

        result->PaneId = SysAllocString(winrt::to_hstring(v.get("pane_id", "").asString()).c_str());
        result->Content = SysAllocString(winrt::to_hstring(v.get("content", "").asString()).c_str());
        result->LineCount = v.get("line_count", 0).asInt();
        result->Truncated = v.get("truncated", false).asBool() ? TRUE : FALSE;
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

        const auto jsonStr = winrt::to_string(page.GetProtocolProcessStatus(winrt::to_hstring(paneIdStr)));
        if (jsonStr.empty())
            continue;

        Json::Value v;
        if (!_parseJson(jsonStr, v))
            continue;

        result->PaneId = SysAllocString(winrt::to_hstring(v.get("pane_id", "").asString()).c_str());
        result->State = SysAllocString(winrt::to_hstring(v.get("state", "unknown").asString()).c_str());
        result->Pid = v.get("pid", 0u).asUInt();
        result->ExitCode = v.get("exit_code", 0).asInt();
        result->HasExitCode = v.isMember("exit_code") && !v["exit_code"].isNull() ? TRUE : FALSE;
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

        const auto jsonStr = winrt::to_string(page.GetProtocolSessionVariable(
            winrt::to_hstring(paneIdStr), winrt::to_hstring(nameStr)));
        if (jsonStr.empty())
            continue;

        Json::Value v;
        if (!_parseJson(jsonStr, v))
            continue;

        result->PaneId = SysAllocString(winrt::to_hstring(v.get("pane_id", "").asString()).c_str());
        result->Name = SysAllocString(winrt::to_hstring(v.get("name", "").asString()).c_str());
        result->Value = SysAllocString(winrt::to_hstring(v.get("value", "").asString()).c_str());
        result->Exists = v.get("exists", false).asBool() ? TRUE : FALSE;
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
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);
    memset(result, 0, sizeof(*result));

    // Build JSON params and delegate to the existing handler.
    Json::Value params;
    if (windowId && SysStringLen(windowId) > 0)
        params["window_id"] = winrt::to_string(std::wstring_view(windowId, SysStringLen(windowId)));
    if (profile && SysStringLen(profile) > 0)
        params["profile"] = winrt::to_string(std::wstring_view(profile, SysStringLen(profile)));
    if (commandline && SysStringLen(commandline) > 0)
        params["commandline"] = winrt::to_string(std::wstring_view(commandline, SysStringLen(commandline)));
    if (title && SysStringLen(title) > 0)
        params["title"] = winrt::to_string(std::wstring_view(title, SysStringLen(title)));
    params["suppress_application_title"] = suppressAppTitle ? true : false;
    params["inject_mcp_credentials"] = injectMcpCredentials ? true : false;
    params["background"] = background ? true : false;

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-create-tab";
    request["method"] = "create_tab";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    const auto& r = response["result"];
    if (r.isNull())
        return E_FAIL;

    result->TabId = SysAllocString(winrt::to_hstring(r.get("tab_id", "").asString()).c_str());
    result->PaneId = SysAllocString(winrt::to_hstring(r.get("pane_id", "").asString()).c_str());
    result->WindowId = SysAllocString(winrt::to_hstring(r.get("window_id", "").asString()).c_str());
    result->Pid = r.get("pid", 0u).asUInt();
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
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);
    memset(result, 0, sizeof(*result));

    Json::Value params;
    if (paneId && SysStringLen(paneId) > 0)
        params["pane_id"] = winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId)));
    if (direction && SysStringLen(direction) > 0)
        params["direction"] = winrt::to_string(std::wstring_view(direction, SysStringLen(direction)));
    params["size"] = size;
    if (profile && SysStringLen(profile) > 0)
        params["profile"] = winrt::to_string(std::wstring_view(profile, SysStringLen(profile)));
    if (commandline && SysStringLen(commandline) > 0)
        params["commandline"] = winrt::to_string(std::wstring_view(commandline, SysStringLen(commandline)));
    params["inject_mcp_credentials"] = injectMcpCredentials ? true : false;
    params["background"] = background ? true : false;

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-split-pane";
    request["method"] = "split_pane";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    const auto& r = response["result"];
    if (r.isNull())
        return E_FAIL;

    result->TabId = SysAllocString(winrt::to_hstring(r.get("tab_id", "").asString()).c_str());
    result->PaneId = SysAllocString(winrt::to_hstring(r.get("pane_id", "").asString()).c_str());
    result->WindowId = SysAllocString(winrt::to_hstring(r.get("window_id", "").asString()).c_str());
    result->Pid = r.get("pid", 0u).asUInt();
    return S_OK;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::ClosePane(BSTR paneId)
try
{
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);

    Json::Value params;
    if (paneId && SysStringLen(paneId) > 0)
        params["pane_id"] = winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId)));

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-close-pane";
    request["method"] = "close_pane";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    if (!response["result"].isNull() && response["error"].isNull())
        return S_OK;
    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SendInput(BSTR paneId, BSTR text)
try
{
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);

    Json::Value params;
    if (paneId && SysStringLen(paneId) > 0)
        params["pane_id"] = winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId)));
    if (text && SysStringLen(text) > 0)
        params["text"] = winrt::to_string(std::wstring_view(text, SysStringLen(text)));

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-send-input";
    request["method"] = "send_input";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    if (!response["result"].isNull() && response["error"].isNull())
        return S_OK;
    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SetSessionVariable(BSTR paneId, BSTR name, BSTR value)
try
{
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);

    Json::Value params;
    if (paneId && SysStringLen(paneId) > 0)
        params["pane_id"] = winrt::to_string(std::wstring_view(paneId, SysStringLen(paneId)));
    if (name && SysStringLen(name) > 0)
        params["name"] = winrt::to_string(std::wstring_view(name, SysStringLen(name)));
    if (value && SysStringLen(value) > 0)
        params["value"] = winrt::to_string(std::wstring_view(value, SysStringLen(value)));

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-set-session-var";
    request["method"] = "set_session_variable";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    if (!response["result"].isNull() && response["error"].isNull())
        return S_OK;
    return E_FAIL;
}
CATCH_RETURN()

STDMETHODIMP TerminalProtocolComServer::SetSettings(BSTR settingsContent, BSTR* backupPath)
try
{
    RETURN_HR_IF_NULL(E_POINTER, backupPath);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);
    *backupPath = nullptr;

    const auto contentStr = settingsContent
        ? winrt::to_string(std::wstring_view(settingsContent, SysStringLen(settingsContent)))
        : std::string{};

    Json::Value params;
    params["settings"] = contentStr;

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-set-settings";
    request["method"] = "set_settings";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    const auto& r = response["result"];
    if (r.isNull())
        return E_FAIL;

    *backupPath = SysAllocString(winrt::to_hstring(r.get("backup_path", "").asString()).c_str());
    return S_OK;
}
CATCH_RETURN()

// ============================================================================
// Interactive
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::QuickPick(BSTR title, UINT32 choiceCount, BSTR* choices,
                                                   BOOL allowFreeInput, BOOL* cancelled, BSTR* selected)
try
{
    RETURN_HR_IF_NULL(E_POINTER, cancelled);
    RETURN_HR_IF_NULL(E_POINTER, selected);
    RETURN_HR_IF_NULL(E_NOT_VALID_STATE, s_handler);
    *cancelled = TRUE;
    *selected = nullptr;

    // Build JSON request params
    Json::Value params;
    if (title && SysStringLen(title) > 0)
        params["title"] = winrt::to_string(std::wstring_view(title, SysStringLen(title)));

    Json::Value choicesArr(Json::arrayValue);
    for (UINT32 i = 0; i < choiceCount; ++i)
    {
        if (choices[i])
            choicesArr.append(winrt::to_string(std::wstring_view(choices[i], SysStringLen(choices[i]))));
    }
    params["choices"] = choicesArr;
    params["allow_free_input"] = allowFreeInput ? true : false;

    Json::Value request;
    request["type"] = "request";
    request["id"] = "com-quick-pick";
    request["method"] = "quick_pick";
    request["params"] = params;

    const auto response = s_handler->HandleRequest(request, _authenticated);
    const auto& r = response["result"];
    if (r.isNull())
        return E_FAIL;

    *cancelled = r.get("cancelled", true).asBool() ? TRUE : FALSE;
    *selected = SysAllocString(winrt::to_hstring(r.get("selected", "").asString()).c_str());
    return S_OK;
}
CATCH_RETURN()

// ============================================================================
// Events
// ============================================================================

STDMETHODIMP TerminalProtocolComServer::PollEvents(UINT32 timeoutMs, UINT32* eventCount, BSTR** events)
try
{
    RETURN_HR_IF_NULL(E_POINTER, eventCount);
    RETURN_HR_IF_NULL(E_POINTER, events);
    *eventCount = 0;
    *events = nullptr;

    if (!_authenticated)
        return E_ACCESSDENIED;

    // Trigger lazy page event registration once per instance
    if (!_eventsInitialized && s_handler)
    {
        _eventsInitialized = true;
        Json::Value capReq;
        capReq["type"] = "request";
        capReq["id"] = "com-poll-init";
        capReq["method"] = "get_capabilities";
        capReq["params"] = Json::objectValue;
        s_handler->HandleRequest(capReq, _authenticated);
    }

    // Wait for events up to timeoutMs
    if (_eventSignal)
    {
        WaitForSingleObject(_eventSignal.get(), timeoutMs);
        // Brief delay to allow batching — avoids tight COM round-trips
        // when events arrive in rapid succession (e.g. VT sequences).
        Sleep(20);
    }

    // Drain the queue
    std::vector<std::string> drained;
    {
        std::lock_guard lock{ _eventMutex };
        drained.swap(_eventQueue);
        if (_eventSignal)
        {
            _eventSignal.ResetEvent();
        }
    }

    if (drained.empty())
        return S_OK;

    *eventCount = static_cast<UINT32>(drained.size());
    *events = static_cast<BSTR*>(CoTaskMemAlloc(drained.size() * sizeof(BSTR)));
    RETURN_HR_IF_NULL(E_OUTOFMEMORY, *events);

    for (UINT32 i = 0; i < drained.size(); ++i)
    {
        (*events)[i] = SysAllocString(winrt::to_hstring(drained[i]).c_str());
    }

    return S_OK;
}
CATCH_RETURN()
