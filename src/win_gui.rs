//! 原生 Win32 窗口。窗口线程管理控件，工作线程只处理文件与转换结果。
// Win32 调用集中在此模块，窗口句柄和传入缓冲区都由调用方在调用期间持有。
#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    cell::RefCell,
    ffi::{OsStr, OsString},
    mem::{size_of, zeroed},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver},
    thread,
};

use pdf_to_word::{
    ConversionOptions, DEFAULT_JPEG_QUALITY, OutputSkipped, PageSelection,
    convert_document_confirming, default_word_output, export_selected_images_confirming,
    read_pdf_page_count,
    latex2word,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        COLOR_WINDOW, COLOR_WINDOWTEXT, CreateFontW, DeleteObject, GetSysColor, GetSysColorBrush,
        HDC, HFONT, RedrawWindow, SetBkMode, SetTextColor, TRANSPARENT, RDW_ALLCHILDREN,
        RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
    },
    System::{
        Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        LibraryLoader::GetModuleHandleW,
    },
    UI::{
        Controls::Dialogs::{
            CommDlgExtendedError, GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER,
            OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
        },
        Controls::{
            EM_SETCUEBANNER, ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, InitCommonControlsEx,
            PBM_SETPOS, PBM_SETRANGE32, PROGRESS_CLASSW, TASKDIALOG_BUTTON, TASKDIALOGCONFIG,
            TASKDIALOGCONFIG_0, TD_WARNING_ICON, TDF_ALLOW_DIALOG_CANCELLATION,
            TDF_POSITION_RELATIVE_TO_WINDOW, TaskDialogIndirect,
        },
        HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi},
        Input::KeyboardAndMouse::EnableWindow,
        Shell::{
            BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, DragAcceptFiles, DragFinish,
            DragQueryFileW, HDROP, SHBrowseForFolderW, SHGetPathFromIDListEx,
        },
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::{PCWSTR, w};

const INTRO: i32 = 101;
const ADD: i32 = 102;
const REMOVE: i32 = 103;
const CLEAR: i32 = 104;
const QUEUE: i32 = 105;
const DETAILS: i32 = 106;
const DIR_LABEL: i32 = 107;
const DIRECTORY: i32 = 108;
const BROWSE: i32 = 109;
const FORMAT_LABEL: i32 = 110;
const FORMAT: i32 = 111;
const START: i32 = 112;
const PROGRESS: i32 = 113;
const STATUS: i32 = 114;
const RESET_DIRECTORY: i32 = 115;
const IMAGE_DPI_LABEL: i32 = 116;
const IMAGE_DPI: i32 = 117;
const JPG_QUALITY_LABEL: i32 = 118;
const JPG_QUALITY: i32 = 119;
const PAGE_RANGE_LABEL: i32 = 120;
const PAGE_RANGE: i32 = 121;
const PAGE_COUNT: i32 = 122;
const OVERWRITE_ALL: i32 = 1001;
const SKIP_ALL: i32 = 1002;
const IMAGE_DPI_OPTIONS: [u32; 6] = [72, 96, 150, 200, 300, 600];
const MAX_IMAGE_DPI: u32 = 600;
const WINDOW_TITLE: PCWSTR = w!("PDF2Word");
const APP_ICON_RESOURCE: PCWSTR = 1_usize as PCWSTR;
const TIMER: usize = 1;
const STATIC_PATH_ELLIPSIS: u32 = 0x8000;

#[derive(Default)]
enum JobStatus {
    #[default]
    Waiting,
    Running,
    Complete,
    Skipped,
    Failed(String),
}

impl JobStatus {
    fn label(&self) -> &str {
        match self {
            Self::Waiting => "待转换",
            Self::Running => "转换中",
            Self::Complete => "已完成",
            Self::Skipped => "已跳过",
            Self::Failed(_) => "失败",
        }
    }

    fn icon(&self) -> &str {
        match self {
            Self::Waiting => "⌛",
            Self::Running => "⚙️",
            Self::Complete => "✅",
            Self::Skipped => "⏭️",
            Self::Failed(_) => "❌",
        }
    }
}

struct Job {
    input: PathBuf,
    output: Option<PathBuf>,
    status: JobStatus,
    page_range: String,
    page_count: Option<PageCount>,
}

type PageCount = Result<usize, String>;

struct PageCountQuery {
    path: PathBuf,
    result: Receiver<PageCount>,
}

enum BatchEvent {
    Started(usize, PathBuf),
    ConfirmOverwrite(PathBuf, mpsc::Sender<bool>),
    Finished(usize, Result<Option<PathBuf>, String>),
}

struct Batch {
    events: Receiver<BatchEvent>,
    count: usize,
    done: usize,
    failures: usize,
    skipped: usize,
    overwrite_all: Option<bool>,
}

impl Batch {
    /// 仅当前批次记住「全部」决定。关闭对话框按跳过当前项处理。
    unsafe fn confirm_overwrite(&mut self, window: HWND, path: &Path) -> bool {
        if let Some(overwrite) = self.overwrite_all {
            return overwrite;
        }
        let decision = overwrite_dialog(window, path);
        self.overwrite_all = match decision {
            OVERWRITE_ALL => Some(true),
            SKIP_ALL => Some(false),
            _ => None,
        };
        matches!(decision, IDYES | OVERWRITE_ALL)
    }
}

#[derive(Default)]
struct State {
    jobs: Vec<Job>,
    batch: Option<Batch>,
    directory: Option<PathBuf>,
    page_count_query: Option<PageCountQuery>,
}

#[derive(Clone, Copy)]
enum OutputFormat {
    Word,
    ImagePdf,
    Png,
    Jpg,
}

impl OutputFormat {
    unsafe fn selected(window: HWND) -> Self {
        match SendMessageW(GetDlgItem(window, FORMAT), CB_GETCURSEL, 0, 0) {
            1 => Self::ImagePdf,
            2 => Self::Png,
            3 => Self::Jpg,
            _ => Self::Word,
        }
    }

    fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpg)
    }
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 启动独立的 Win32 消息循环。
pub fn run() {
    // SAFETY: 所有窗口创建、消息派发、控件访问和销毁都发生在本线程。
    unsafe {
        let com = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let result = run_window();
        if let Err(error) = result {
            MessageBoxW(
                null_mut(),
                wide(error).as_ptr(),
                WINDOW_TITLE,
                MB_OK | MB_ICONERROR,
            );
        }
        if com >= 0 {
            CoUninitialize();
        }
    }
}

unsafe fn run_window() -> Result<(), String> {
    let common = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_PROGRESS_CLASS,
    };
    InitCommonControlsEx(&common);
    let instance = GetModuleHandleW(null());
    let class_name = w!("PDF2WordWindow");
    let system_dpi = GetDpiForSystem().max(96);
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: LoadIconW(instance, APP_ICON_RESOURCE),
        hIconSm: LoadImageW(
            instance,
            APP_ICON_RESOURCE,
            IMAGE_ICON,
            GetSystemMetricsForDpi(SM_CXSMICON, system_dpi),
            GetSystemMetricsForDpi(SM_CYSMICON, system_dpi),
            LR_SHARED,
        ),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: GetSysColorBrush(COLOR_WINDOW),
        lpszClassName: class_name,
        ..zeroed()
    };
    if RegisterClassExW(&class) == 0 {
        return Err(format!("无法注册窗口：{}", std::io::Error::last_os_error()));
    }
    let system_dpi = system_dpi as i32;
    let mut work_area: RECT = zeroed();
    let has_work_area =
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work_area as *mut RECT).cast(), 0) != 0;
    let mut width = 850 * system_dpi / 96;
    let mut height = 560 * system_dpi / 96;
    if has_work_area {
        width = width.min(work_area.right - work_area.left);
        height = height.min(work_area.bottom - work_area.top);
    }
    let window = CreateWindowExW(
        WS_EX_CONTROLPARENT,
        class_name,
        WINDOW_TITLE,
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        width,
        height,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return Err(format!("无法创建窗口：{}", std::io::Error::last_os_error()));
    }

    // 状态由消息循环持有，窗口过程借用状态。RefCell 可防止模态对话框重入时重复借用。
    let state = Box::new(RefCell::new(State::default()));
    SetWindowLongPtrW(
        window,
        GWLP_USERDATA,
        (&*state as *const RefCell<State>) as isize,
    );
    let scale = GetDpiForWindow(window).max(96) as i32;
    let font = create_font(16 * scale / 96);
    if let Err(error) = create_controls(window, font) {
        DestroyWindow(window);
        DeleteObject(font);
        return Err(error);
    }
    DragAcceptFiles(window, 1);
    SetTimer(window, TIMER, 60, None);
    layout(window);
    state.borrow_mut().add_files(
        window,
        std::env::args_os().skip(1).map(PathBuf::from).collect(),
    );
    ShowWindow(window, SW_SHOW);
    let mut message: MSG = zeroed();
    let mut result = Ok(());
    loop {
        let code = GetMessageW(&mut message, null_mut(), 0, 0);
        if code <= 0 {
            if code < 0 {
                result = Err("读取窗口消息失败".to_owned());
                DestroyWindow(window);
            }
            break;
        }
        if IsDialogMessageW(window, &message) == 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    DeleteObject(font);
    result
}

unsafe fn create_font(size: i32) -> HFONT {
    CreateFontW(-size, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI"))
}

unsafe fn create_controls(window: HWND, font: HFONT) -> Result<(), String> {
    let controls = [
        (
            INTRO,
            w!("STATIC"),
            w!("拖入 PDF、图像或 LaTeX 源文件，或点击添加文件。"),
            0,
            0,
        ),
        (ADD, w!("BUTTON"), w!("添加文件…"), WS_TABSTOP, 0),
        (REMOVE, w!("BUTTON"), w!("移除所选"), WS_TABSTOP, 0),
        (CLEAR, w!("BUTTON"), w!("清空队列"), WS_TABSTOP, 0),
        (
            QUEUE,
            w!("LISTBOX"),
            w!(""),
            WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32,
            WS_EX_CLIENTEDGE,
        ),
        (DETAILS, w!("BUTTON"), w!("文件详情"), WS_TABSTOP, 0),
        (DIR_LABEL, w!("STATIC"), w!("输出目录"), 0, 0),
        (
            DIRECTORY,
            w!("STATIC"),
            w!("原文件所在目录"),
            STATIC_PATH_ELLIPSIS,
            0,
        ),
        (BROWSE, w!("BUTTON"), w!("选择目录…"), WS_TABSTOP | BS_DEFPUSHBUTTON as u32, 0),
        (FORMAT_LABEL, w!("STATIC"), w!("输出格式"), 0, 0),
        (
            FORMAT,
            w!("COMBOBOX"),
            w!(""),
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            0,
        ),
        (IMAGE_DPI_LABEL, w!("STATIC"), w!("DPI"), 0, 0),
        (
            IMAGE_DPI,
            w!("COMBOBOX"),
            w!(""),
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWN as u32 | CBS_AUTOHSCROLL as u32,
            0,
        ),
        (JPG_QUALITY_LABEL, w!("STATIC"), w!("质量"), 0, 0),
        (
            JPG_QUALITY,
            w!("EDIT"),
            w!(""),
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
        ),
        (
            RESET_DIRECTORY,
            w!("BUTTON"),
            w!("输出到原文件目录"),
            WS_TABSTOP,
            0,
        ),
        (PAGE_RANGE_LABEL, w!("STATIC"), w!("页码范围"), 0, 0),
        (
            PAGE_RANGE,
            w!("EDIT"),
            w!(""),
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
        ),
        (PAGE_COUNT, w!("STATIC"), w!("请选择 PDF"), 0, 0),
        (
            START,
            w!("BUTTON"),
            w!("开始转换"),
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            0,
        ),
        (PROGRESS, PROGRESS_CLASSW, w!(""), 0, 0),
        (STATUS, w!("STATIC"), w!("等待添加文件"), 0, 0),
    ];
    for (id, class, text, style, ex_style) in controls {
        let handle = CreateWindowExW(
            ex_style,
            class,
            text,
            WS_CHILD
                | if matches!(
                    id,
                    PROGRESS
                        | IMAGE_DPI_LABEL
                        | IMAGE_DPI
                        | JPG_QUALITY_LABEL
                        | JPG_QUALITY
                ) {
                    0
                } else {
                    WS_VISIBLE
                }
                | style,
            0,
            0,
            10,
            10,
            window,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        if handle.is_null() {
            return Err(format!(
                "无法创建窗口控件：{}",
                std::io::Error::last_os_error()
            ));
        }
        SendMessageW(handle, WM_SETFONT, font as WPARAM, 0);
    }
    let formats = GetDlgItem(window, FORMAT);
    SendMessageW(
        formats,
        CB_ADDSTRING,
        0,
        w!("Word 文档（自动识别）") as LPARAM,
    );
    SendMessageW(formats, CB_ADDSTRING, 0, w!("图像 PDF") as LPARAM);
    SendMessageW(formats, CB_ADDSTRING, 0, w!("PNG 图片") as LPARAM);
    SendMessageW(formats, CB_ADDSTRING, 0, w!("JPG 图片") as LPARAM);
    SendMessageW(formats, CB_SETCURSEL, 0, 0);
    let resolutions = GetDlgItem(window, IMAGE_DPI);
    for dpi in IMAGE_DPI_OPTIONS {
        let label = wide(dpi.to_string());
        SendMessageW(resolutions, CB_ADDSTRING, 0, label.as_ptr() as LPARAM);
    }
    SendMessageW(resolutions, CB_SETCURSEL, 2, 0);
    set_text(window, JPG_QUALITY, DEFAULT_JPEG_QUALITY.to_string());
    SendMessageW(
        GetDlgItem(window, PAGE_RANGE),
        EM_SETCUEBANNER,
        1,
        w!("例如 1-3,5，留空导出全部 PDF 页面") as LPARAM,
    );
    Ok(())
}

unsafe fn layout(window: HWND) {
    let dpi = GetDpiForWindow(window).max(96) as i32;
    let scale = |value: i32| value * dpi / 96;
    let mut rect: RECT = zeroed();
    GetClientRect(window, &mut rect);
    let w = rect.right * 96 / dpi;
    let h = rect.bottom * 96 / dpi;
    if w < 500 || h < 355 {
        return;
    }
    let place = |id, x, y, width, height| {
        MoveWindow(
            GetDlgItem(window, id),
            scale(x),
            scale(y),
            scale(width),
            scale(height),
            1,
        );
    };
    place(INTRO, 22, 19, w - 44, 21);
    place(ADD, 22, 49, 112, 30);
    place(REMOVE, 144, 49, 102, 30);
    place(CLEAR, 256, 49, 102, 30);
    place(DETAILS, w - 122, 49, 100, 30);
    place(QUEUE, 22, 89, w - 44, h - 261);
    place(DIR_LABEL, 22, h - 151, 72, 23);
    place(DIRECTORY, 100, h - 151, w - 182 - 110, 23);
    place(BROWSE, w - 182, h - 156, 160, 32);
    place(RESET_DIRECTORY, w - 182, h - 117, 160, 32);
    place(FORMAT_LABEL, 22, h - 112, 72, 22);
    place(FORMAT, 100, h - 117, 247, 150);
    place(IMAGE_DPI_LABEL, 367, h - 112, 34, 22);
    place(IMAGE_DPI, 367 + 38, h - 117, 78, 195);
    place(JPG_QUALITY_LABEL, 367 + 133, h - 112, 36, 22);
    place(JPG_QUALITY, 367 + 176, h - 117, 52, 26);
    place(PAGE_RANGE_LABEL, 22, h - 73, 72, 22);
    place(PAGE_RANGE, 100, h - 78, 247, 26);
    place(PAGE_COUNT, 360, h - 73, 151, 22);
    place(START, w - 182, h - 80, 160, 32);
    place(PROGRESS, w - 222, h - 29, 200, 9);
    place(STATUS, 22, h - 34, w - 44, 22);
}

/// 处理窗口事件。状态借用只在本次消息处理期间有效。
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        KillTimer(window, TIMER);
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        PostQuitMessage(0);
        return 0;
    }
    if message == WM_CTLCOLORSTATIC {
        // 说明和状态文字与窗口共用系统底色，避免静态控件绘制灰色条。
        let dc = wparam as HDC;
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, GetSysColor(COLOR_WINDOWTEXT));
        return GetSysColorBrush(COLOR_WINDOW) as LRESULT;
    }
    if message == WM_GETMINMAXINFO {
        let limits = &mut *(lparam as *mut MINMAXINFO);
        let dpi = GetDpiForWindow(window).max(96) as i32;
        limits.ptMinTrackSize.x = 700 * dpi / 96;
        limits.ptMinTrackSize.y = 450 * dpi / 96;
        return 0;
    }
    let ptr = GetWindowLongPtrW(window, GWLP_USERDATA) as *const RefCell<State>;
    if let Some(state) = ptr.as_ref()
        && let Ok(mut state) = state.try_borrow_mut()
    {
        match message {
            WM_SIZE => {
                // 暂停父窗口和子控件的逐个重绘，完成布局后一次性刷新，避免
                // MoveWindow 连续触发重绘造成按钮和静态控件残影。
                SendMessageW(window, WM_SETREDRAW, 0, 0);
                layout(window);
                SendMessageW(window, WM_SETREDRAW, 1, 0);
                RedrawWindow(
                    window,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
                return 0;
            }
            WM_TIMER if wparam == TIMER => {
                state.poll_page_count(window);
                state.poll_batch(window);
                return 0;
            }
            WM_DROPFILES => {
                let files = dropped_files(wparam as HDROP);
                if state.batch.is_none() {
                    state.add_files(window, files);
                }
                return 0;
            }
            WM_COMMAND => {
                let id = (wparam & 0xffff) as i32;
                let notification = ((wparam >> 16) & 0xffff) as u32;
                let settings_changed = match id {
                    FORMAT => notification == CBN_SELCHANGE,
                    IMAGE_DPI => matches!(notification, CBN_SELCHANGE | CBN_EDITCHANGE),
                    JPG_QUALITY => notification == EN_CHANGE,
                    _ => false,
                };
                if settings_changed && state.batch.is_none() {
                    state.reset_completed(window);
                    return 0;
                }
                if id == PAGE_RANGE && notification == EN_CHANGE && state.batch.is_none() {
                    state.save_page_range(window);
                    return 0;
                }
                if id == QUEUE && notification == LBN_SELCHANGE {
                    state.update_controls(window);
                    state.update_page_editor(window);
                    return 0;
                }
                if id == QUEUE && notification == LBN_DBLCLK {
                    state.show_details(window);
                    return 0;
                }
                if notification == BN_CLICKED && state.batch.is_none() {
                    match id {
                        ADD => match choose_files(window) {
                            Ok(files) if !files.is_empty() => state.add_files(window, files),
                            Err(error) => set_text(window, STATUS, error),
                            _ => {}
                        },
                        REMOVE => {
                            if let Some(index) = selected_index(window)
                                && index < state.jobs.len()
                            {
                                state.jobs.remove(index);
                                state.refresh(window);
                            }
                        }
                        CLEAR => {
                            state.jobs.clear();
                            state.refresh(window);
                        }
                        BROWSE => {
                            if let Some(path) = choose_directory(window) {
                                state.set_directory(window, Some(path));
                            }
                        }
                        RESET_DIRECTORY => state.set_directory(window, None),
                        DETAILS => state.show_details(window),
                        START => state.start_batch(window),
                        _ => {}
                    }
                }
                return 0;
            }
            WM_CLOSE if state.batch.is_some() => {
                set_text(window, STATUS, "批量转换正在运行，完成后可关闭窗口。");
                return 0;
            }
            _ => {}
        }
    } else if message == WM_DROPFILES {
        DragFinish(wparam as HDROP);
        return 0;
    }
    DefWindowProcW(window, message, wparam, lparam)
}

unsafe fn set_text(window: HWND, id: i32, text: impl AsRef<OsStr>) {
    SetWindowTextW(GetDlgItem(window, id), wide(text).as_ptr());
}

/// 用原生任务对话框区分单项决定和本次批量决定。
unsafe fn overwrite_dialog(window: HWND, path: &Path) -> i32 {
    let instruction = if path.is_dir() {
        w!("图片输出目录已存在。是否替换本次导出的同名图片？")
    } else {
        w!("输出文件已存在。是否覆盖？")
    };
    let location = wide(path.as_os_str());
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: IDYES,
            pszButtonText: w!("覆盖"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDNO,
            pszButtonText: w!("跳过"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: OVERWRITE_ALL,
            pszButtonText: w!("全部覆盖"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: SKIP_ALL,
            pszButtonText: w!("全部跳过"),
        },
    ];
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: window,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        pszWindowTitle: w!("PDF转换 · 确认覆盖"),
        Anonymous1: TASKDIALOGCONFIG_0 {
            pszMainIcon: TD_WARNING_ICON,
        },
        pszMainInstruction: instruction,
        pszContent: location.as_ptr(),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: IDNO,
        pszFooter: w!("「全部覆盖」和「全部跳过」仅对本次批量转换生效。"),
        cxWidth: 320,
        ..TASKDIALOGCONFIG::default()
    };
    let mut decision = IDNO;
    if TaskDialogIndirect(&config, &mut decision, null_mut(), null_mut()) < 0 {
        return IDNO;
    }
    decision
}

unsafe fn read_text(window: HWND, id: i32) -> String {
    let field = GetDlgItem(window, id);
    let mut text = vec![0; GetWindowTextLengthW(field) as usize + 1];
    let length = GetWindowTextW(field, text.as_mut_ptr(), text.len() as i32);
    String::from_utf16_lossy(&text[..length as usize])
}

fn parse_page_range(input: &str) -> Result<Option<PageSelection>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    input
        .parse()
        .map(Some)
        .map_err(|error| format!("页码范围无效，请输入类似 1-3,5 的格式。{error}"))
}

/// 读取可编辑设置中的整数。过长的输入也按无效值处理。
unsafe fn read_number(window: HWND, id: i32) -> Option<u32> {
    let mut text = [0; 32];
    let length = GetWindowTextW(GetDlgItem(window, id), text.as_mut_ptr(), text.len() as i32);
    if length <= 0 || length as usize >= text.len() - 1 {
        return None;
    }
    String::from_utf16_lossy(&text[..length as usize])
        .trim()
        .parse()
        .ok()
}

unsafe fn selected_index(window: HWND) -> Option<usize> {
    usize::try_from(SendMessageW(GetDlgItem(window, QUEUE), LB_GETCURSEL, 0, 0)).ok()
}

/// 读取拖放句柄中的全部 Unicode 路径，并在返回前释放句柄。
unsafe fn dropped_files(drop: HDROP) -> Vec<PathBuf> {
    let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
    let mut files = Vec::new();
    for index in 0..count {
        let length = DragQueryFileW(drop, index, null_mut(), 0);
        if length > 0 {
            let mut path = vec![0; length as usize + 1];
            if DragQueryFileW(drop, index, path.as_mut_ptr(), length + 1) == length {
                files.push(PathBuf::from(OsString::from_wide(&path[..length as usize])));
            }
        }
    }
    DragFinish(drop);
    files
}

unsafe fn choose_files(window: HWND) -> Result<Vec<PathBuf>, String> {
    let mut buffer = vec![0u16; 65_536];
    let mut dialog: OPENFILENAMEW = zeroed();
    dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = window;
    dialog.lpstrTitle = w!("选择待转换的 PDF、图像或 LaTeX 源文件");
    dialog.lpstrFilter = w!("PDF文件(*.pdf)\0*.pdf\0图像文件(*.png;*.jpg;*.jpeg;*.bmp)\0*.png;*.jpg;*.jpeg;*.bmp\0LaTeX文件(*.tex)\0*.tex\0所有文件(*.*)\0*.*\0");
    dialog.lpstrFile = buffer.as_mut_ptr();
    dialog.nMaxFile = buffer.len() as u32;
    dialog.Flags = OFN_EXPLORER
        | OFN_ALLOWMULTISELECT
        | OFN_FILEMUSTEXIST
        | OFN_PATHMUSTEXIST
        | OFN_NOCHANGEDIR;
    if GetOpenFileNameW(&mut dialog) == 0 {
        let code = CommDlgExtendedError();
        return if code == 0 {
            Ok(Vec::new())
        } else {
            Err(format!("无法读取所选文件，错误代码 {code}。"))
        };
    }
    let mut paths = buffer
        .split(|&code| code == 0)
        .take_while(|path| !path.is_empty())
        .map(|path| PathBuf::from(OsString::from_wide(path)));
    let Some(first) = paths.next() else {
        return Ok(Vec::new());
    };
    let names: Vec<_> = paths.collect();
    if names.is_empty() {
        Ok(vec![first])
    } else {
        Ok(names.into_iter().map(|name| first.join(name)).collect())
    }
}

unsafe fn choose_directory(window: HWND) -> Option<PathBuf> {
    let mut dialog: BROWSEINFOW = zeroed();
    dialog.hwndOwner = window;
    dialog.lpszTitle = w!("选择批量转换的输出目录");
    dialog.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
    let selection = SHBrowseForFolderW(&dialog);
    if selection.is_null() {
        return None;
    }
    let mut buffer = vec![0; 32_768];
    let found = SHGetPathFromIDListEx(selection, buffer.as_mut_ptr(), buffer.len() as u32, 0);
    CoTaskMemFree(selection.cast());
    if found == 0 {
        return None;
    }
    let length = buffer.iter().position(|&value| value == 0)?;
    Some(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            ["pdf", "png", "jpg", "jpeg", "bmp", "tex", "latex"]
                .iter()
                .any(|format| extension.eq_ignore_ascii_case(format))
        })
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn is_latex(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("tex") || ext.eq_ignore_ascii_case("latex"))
}

/// 默认保留同名 DOCX。源文件与目标格式相同则给输出名加后缀。
fn destination(input: &Path, directory: Option<&Path>, format: OutputFormat) -> PathBuf {
    let mut output = default_word_output(input);
    let extension = match format {
        OutputFormat::Word => "docx",
        OutputFormat::ImagePdf => "pdf",
        OutputFormat::Png => "png",
        OutputFormat::Jpg => "jpg",
    };
    if input
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
    {
        let mut name = input
            .file_stem()
            .map_or_else(OsString::new, OsStr::to_os_string);
        name.push(format!("_image.{extension}"));
        output.set_file_name(name);
    } else {
        output.set_extension(extension);
    }
    match (directory, output.file_name()) {
        (Some(directory), Some(name)) => directory.join(name),
        _ => output,
    }
}

impl State {
    /// 导出设置变化后，可以直接用当前队列再次导出。
    unsafe fn reset_completed(&mut self, window: HWND) {
        for job in &mut self.jobs {
            if matches!(job.status, JobStatus::Complete) {
                job.status = JobStatus::Waiting;
                job.output = None;
            }
        }
        self.refresh(window);
    }

    unsafe fn add_files(&mut self, window: HWND, files: Vec<PathBuf>) {
        let mut ignored = 0;
        for input in files {
            if supported(&input)
                && input.is_file()
                && !self.jobs.iter().any(|job| job.input == input)
            {
                self.jobs.push(Job {
                    input,
                    output: None,
                    status: JobStatus::Waiting,
                    page_range: String::new(),
                    page_count: None,
                });
            } else {
                ignored += 1;
            }
        }
        self.refresh(window);
        if ignored > 0 {
            set_text(
                window,
                STATUS,
                format!(
                    "队列中有 {} 个文件，已跳过 {ignored} 个重复或不支持的项目。",
                    self.jobs.len()
                ),
            );
        }
    }

    unsafe fn refresh(&mut self, window: HWND) {
        let queue = GetDlgItem(window, QUEUE);
        let selection = selected_index(window);
        SendMessageW(queue, WM_SETREDRAW, 0, 0);
        SendMessageW(queue, LB_RESETCONTENT, 0, 0);
        for job in &self.jobs {
            let label = wide(format!("{}  {}", job.status.icon(), job.input.display()));
            SendMessageW(queue, LB_ADDSTRING, 0, label.as_ptr() as LPARAM);
        }
        if !self.jobs.is_empty() {
            let index = selection.map_or(0, |index| index.min(self.jobs.len() - 1));
            SendMessageW(queue, LB_SETCURSEL, index, 0);
        }
        SendMessageW(queue, WM_SETREDRAW, 1, 0);
        windows_sys::Win32::Graphics::Gdi::InvalidateRect(queue, null(), 1);
        self.update_controls(window);
        self.update_page_editor(window);
        if self.batch.is_none() {
            set_text(
                window,
                STATUS,
                format!("队列中有 {} 个文件。", self.jobs.len()),
            );
        }
    }

    /// 将输入即时保存到所选任务，切换文件时恢复原来的文本。
    unsafe fn save_page_range(&mut self, window: HWND) {
        let Some(job) = selected_index(window).and_then(|index| self.jobs.get_mut(index)) else {
            return;
        };
        let text = read_text(window, PAGE_RANGE);
        if !is_pdf(&job.input) || job.page_range == text {
            return;
        }
        job.page_range = text;
        if !matches!(job.status, JobStatus::Waiting) {
            job.status = JobStatus::Waiting;
            job.output = None;
            self.refresh(window);
        }
    }

    /// 按需在后台查询页数，避免打开较大的 PDF 时阻塞窗口。
    unsafe fn update_page_editor(&mut self, window: HWND) {
        let job = selected_index(window).and_then(|index| self.jobs.get_mut(index));
        let (range, label) = match job {
            Some(job) if is_pdf(&job.input) => {
                if job.page_count.is_none() && self.page_count_query.is_none() {
                    let path = job.input.clone();
                    let input = path.clone();
                    let (send, result) = mpsc::channel();
                    match thread::Builder::new()
                        .name("pdf2word-page-count".to_owned())
                        .spawn(move || {
                            let count =
                                read_pdf_page_count(&input).map_err(|error| format!("{error:#}"));
                            let _ = send.send(count);
                        }) {
                        Ok(_) => self.page_count_query = Some(PageCountQuery { path, result }),
                        Err(error) => job.page_count = Some(Err(error.to_string())),
                    }
                }
                let label = match &job.page_count {
                    Some(Ok(count)) => format!("共 {count} 页"),
                    Some(Err(_)) => "无法读取总页数".to_owned(),
                    None => "正在读取总页数…".to_owned(),
                };
                (job.page_range.as_str(), label)
            }
            Some(job) if is_latex(&job.input) => ("", "LaTeX 无需设置页码".to_owned()),
            Some(_) => ("", "图片无需设置页码".to_owned()),
            None => ("", "请选择 PDF".to_owned()),
        };
        if read_text(window, PAGE_RANGE) != range {
            set_text(window, PAGE_RANGE, range);
        }
        set_text(window, PAGE_COUNT, label);
    }

    unsafe fn poll_page_count(&mut self, window: HWND) {
        let Some(query) = &self.page_count_query else {
            return;
        };
        let result = match query.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("读取页数的线程意外结束".to_owned()),
        };
        if let Some(job) = self.jobs.iter_mut().find(|job| job.input == query.path) {
            job.page_count = Some(result);
        }
        self.page_count_query = None;
        self.update_page_editor(window);
    }

    unsafe fn show_details(&self, window: HWND) {
        if let Some(job) = selected_index(window).and_then(|index| self.jobs.get(index)) {
            let mut text = format!(
                "输入  {}\r\n状态  {}",
                job.input.display(),
                job.status.label()
            );
            if let Some(output) = &job.output {
                text.push_str(&format!("\r\n输出  {}", output.display()));
            }
            if let JobStatus::Failed(error) = &job.status {
                text.push_str(&format!("\r\n原因  {error}"));
            } else if let Some(Err(error)) = &job.page_count {
                text.push_str(&format!("\r\n总页数读取失败  {error}"));
            }
            MessageBoxW(
                window,
                wide(text).as_ptr(),
                w!("PDF转换 · 文件详情"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    unsafe fn set_directory(&mut self, window: HWND, directory: Option<PathBuf>) {
        self.directory = directory;
        let label = self
            .directory
            .as_ref()
            .map_or(OsStr::new("原文件所在目录"), |path| path.as_os_str());
        set_text(window, DIRECTORY, label);
        ShowWindow(
            GetDlgItem(window, RESET_DIRECTORY),
            SW_SHOW,
        );
        layout(window);
    }

    unsafe fn update_controls(&self, window: HWND) {
        let idle = self.batch.is_none();
        for id in [ADD, BROWSE, RESET_DIRECTORY, FORMAT] {
            EnableWindow(GetDlgItem(window, id), idle as i32);
        }
        let format = OutputFormat::selected(window);
        for (id, visible) in [
            (IMAGE_DPI_LABEL, format.is_image()),
            (IMAGE_DPI, format.is_image()),
            (JPG_QUALITY_LABEL, matches!(format, OutputFormat::Jpg)),
            (JPG_QUALITY, matches!(format, OutputFormat::Jpg)),
        ] {
            ShowWindow(
                GetDlgItem(window, id),
                if visible { SW_SHOW } else { SW_HIDE },
            );
        }
        for id in [IMAGE_DPI, JPG_QUALITY] {
            EnableWindow(GetDlgItem(window, id), idle as i32);
        }
        for id in [CLEAR, START] {
            EnableWindow(
                GetDlgItem(window, id),
                (idle && !self.jobs.is_empty()) as i32,
            );
        }
        let job = selected_index(window).and_then(|index| self.jobs.get(index));
        let selected = job.is_some();
        EnableWindow(
            GetDlgItem(window, PAGE_RANGE),
            (idle && job.is_some_and(|job| is_pdf(&job.input))) as i32,
        );
        EnableWindow(GetDlgItem(window, REMOVE), (idle && selected) as i32);
        EnableWindow(GetDlgItem(window, DETAILS), selected as i32);
        ShowWindow(
            GetDlgItem(window, PROGRESS),
            if idle { SW_HIDE } else { SW_SHOW },
        );
    }

    /// 把待处理文件移交工作线程，转换失败后继续处理后续文件。
    unsafe fn start_batch(&mut self, window: HWND) {
        let directory = self.directory.as_deref();
        if directory.as_ref().is_some_and(|path| !path.is_dir()) {
            set_text(
                window,
                STATUS,
                "输出目录不存在。请选择已有目录，或切换回原目录。",
            );
            return;
        }
        let format = OutputFormat::selected(window);
        let options = ConversionOptions::default();
        let image_dpi = if format.is_image() {
            let Some(dpi) =
                read_number(window, IMAGE_DPI).filter(|dpi| (1..=MAX_IMAGE_DPI).contains(dpi))
            else {
                set_text(
                    window,
                    STATUS,
                    format!("请输入 1–{MAX_IMAGE_DPI} 之间的整数 DPI。"),
                );
                return;
            };
            dpi
        } else {
            options.dpi
        };
        let jpeg_quality = if matches!(format, OutputFormat::Jpg) {
            let Some(quality) =
                read_number(window, JPG_QUALITY).filter(|quality| (1..=100).contains(quality))
            else {
                set_text(window, STATUS, "请输入 1–100 之间的整数作为 JPG 质量。");
                return;
            };
            quality as u8
        } else {
            DEFAULT_JPEG_QUALITY
        };
        let pending: Vec<_> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| !matches!(job.status, JobStatus::Complete))
            .map(|(index, job)| {
                (
                    index,
                    job.input.clone(),
                    destination(&job.input, directory, format),
                    job.page_range.clone(),
                )
            })
            .collect();
        if pending.is_empty() {
            set_text(
                window,
                STATUS,
                "没有待转换文件。可添加文件，或移除后重新加入。",
            );
            return;
        }
        let count = pending.len();
        let (send, events) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("pdf2word-batch".to_owned())
            .spawn(move || {
                for (index, input, output, range) in pending {
                    if send
                        .send(BatchEvent::Started(index, output.clone()))
                        .is_err()
                    {
                        break;
                    }
                    let result = match std::panic::catch_unwind(|| {
                        let pages = parse_page_range(&range)?;
                        let options = ConversionOptions {
                            pages,
                            ..ConversionOptions::default()
                        };
                        let mut confirm = |path: &Path| {
                            let (answer, response) = mpsc::channel();
                            send.send(BatchEvent::ConfirmOverwrite(path.to_path_buf(), answer))
                                .is_ok()
                                && response.recv().unwrap_or(false)
                        };
                        let result = match format {
                            OutputFormat::Png | OutputFormat::Jpg => {
                                export_selected_images_confirming(
                                    &input,
                                    &output,
                                    image_dpi,
                                    jpeg_quality,
                                    options.pages.as_ref(),
                                    Some(&mut confirm),
                                )
                            }
                            _ if is_latex(&input) => {
                                if output.exists() {
                                    let (answer, response) = mpsc::channel();
                                    if send.send(BatchEvent::ConfirmOverwrite(output.clone(), answer)).is_err() || !response.recv().unwrap_or(false) {
                                        return Ok(None);
                                    }
                                }
                                latex2word::convert(&input, &output, true).map(|_| output)
                            }
                            _ => convert_document_confirming(
                                &input,
                                &output,
                                &options,
                                Some(&mut confirm),
                            )
                            .map(|_| output),
                        };
                        match result {
                            Ok(path) => Ok(Some(path)),
                            Err(error) if error.is::<OutputSkipped>() => Ok(None),
                            Err(error) => Err(format!("{error:#}")),
                        }
                    }) {
                        Ok(result) => result,
                        Err(_) => Err("此文件的转换引擎异常结束。".to_owned()),
                    };
                    if send.send(BatchEvent::Finished(index, result)).is_err() {
                        break;
                    }
                }
            });
        if let Err(error) = worker {
            set_text(window, STATUS, format!("无法启动转换任务：{error}"));
            return;
        }
        self.batch = Some(Batch {
            events,
            count,
            done: 0,
            failures: 0,
            skipped: 0,
            overwrite_all: None,
        });
        SendMessageW(
            GetDlgItem(window, PROGRESS),
            PBM_SETRANGE32,
            0,
            count as LPARAM,
        );
        SendMessageW(GetDlgItem(window, PROGRESS), PBM_SETPOS, 0, 0);
        self.update_controls(window);
        set_text(window, STATUS, format!("正在转换 0 / {count}"));
    }

    /// 从消息队列读取进度，所有控件更新仍在窗口线程进行。
    unsafe fn poll_batch(&mut self, window: HWND) {
        let Some(batch) = &mut self.batch else {
            return;
        };
        let mut changed = false;
        let mut disconnected = false;
        loop {
            match batch.events.try_recv() {
                Ok(BatchEvent::Started(index, output)) => {
                    self.jobs[index].output = Some(output);
                    self.jobs[index].status = JobStatus::Running;
                    changed = true;
                }
                Ok(BatchEvent::ConfirmOverwrite(path, response)) => {
                    let _ = response.send(batch.confirm_overwrite(window, &path));
                }
                Ok(BatchEvent::Finished(index, result)) => {
                    self.jobs[index].status = match result {
                        Ok(Some(output)) => {
                            self.jobs[index].output = Some(output);
                            JobStatus::Complete
                        }
                        Ok(None) => {
                            batch.skipped += 1;
                            JobStatus::Skipped
                        }
                        Err(error) => {
                            batch.failures += 1;
                            JobStatus::Failed(error)
                        }
                    };
                    batch.done += 1;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let (count, done, failures, skipped) =
            (batch.count, batch.done, batch.failures, batch.skipped);
        if disconnected || done == count {
            if done < count {
                for job in &mut self.jobs {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed("转换线程意外结束".to_owned());
                    }
                }
            }
            self.batch = None;
            self.refresh(window);
            SendMessageW(GetDlgItem(window, PROGRESS), PBM_SETPOS, done, 0);
            let successful = done - failures - skipped;
            let failed = failures + count - done;
            let status = if skipped == 0 {
                format!("批量结束：成功 {successful}，失败 {failed}。")
            } else {
                format!("批量结束：成功 {successful}，失败 {failed}，跳过 {skipped}。")
            };
            set_text(window, STATUS, status);
        } else if changed {
            self.refresh(window);
            SendMessageW(GetDlgItem(window, PROGRESS), PBM_SETPOS, done, 0);
            set_text(window, STATUS, format!("正在转换 {done} / {count}"));
        }
    }
}
