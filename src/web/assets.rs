//! 静态资源：首页与 frontend/ 下的 css/js（#54 拆出的前端）。
//!
//! 资源编译进二进制，部署仍然是一个可执行文件。

use tiny_http::Response;

use crate::web::http::header;

pub(crate) fn respond_static(
    request: tiny_http::Request,
    body: &str,
    content_type: &str,
) -> Result<(), String> {
    let resp = Response::from_string(body)
        .with_header(header("Content-Type", content_type))
        .with_header(header("Referrer-Policy", "no-referrer"))
        // 不设这个头，浏览器会一直用缓存的旧页面：改了按钮逻辑用户也看不到，
        // 点了还是旧行为（表现为「退出键没反应」）。
        .with_header(header(
            "Cache-Control",
            "no-store, no-cache, must-revalidate, max-age=0",
        ))
        .with_header(header("Pragma", "no-cache"))
        .with_header(header("Expires", "0"));
    request.respond(resp).map_err(|e| e.to_string())
}

/// Web 控制台首页骨架。样式与脚本经 `<link>` / `<script src>` 引用
/// `STATIC_ASSETS` 里的本地静态路由（与骨架外链 unpkg 的 Leaflet 同一模式），
/// 引用与路由的双向一致性由 `skeleton_asset_references_and_routes_agree` 钉住。
pub(crate) const INDEX_HTML: &str = include_str!("frontend/index.html");

/// 本地静态资源表 `(路由路径, 内容, Content-Type)`，是前端拆分文件的唯一清单。
///
/// 浏览器按骨架里的引用逐个拉取；js 文件之间的执行顺序由 index.html 骨架里
/// `<script src>` 的出现顺序决定——顶层 const/let 不提升，generate.js 等
/// 后面文件的顶层语句直接引用 core.js 的绑定（如 `$('runBtn')`），骨架顺序
/// 不能乱。本表的行序无关紧要。
pub(crate) const STATIC_ASSETS: &[(&str, &str, &str)] = &[
    (
        "/console.css",
        include_str!("frontend/console.css"),
        "text/css; charset=utf-8",
    ),
    (
        "/js/core.js",
        include_str!("frontend/js/core.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "/js/generate.js",
        include_str!("frontend/js/generate.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "/js/map.js",
        include_str!("frontend/js/map.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "/js/draw.js",
        include_str!("frontend/js/draw.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "/js/records.js",
        include_str!("frontend/js/records.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "/js/boot.js",
        include_str!("frontend/js/boot.js"),
        "application/javascript; charset=utf-8",
    ),
];

#[cfg(test)]
mod index_html_tests {
    use super::{INDEX_HTML, STATIC_ASSETS};

    /// 供 contains 断言使用：全部 JS 资产按路由表顺序拼接。这些断言只关心
    /// 包含关系，对顺序不敏感；断言目标按文件归属改到这里，避免对着骨架
    /// 断言一个实际存在于 js 文件里的内容。
    fn console_js() -> String {
        STATIC_ASSETS
            .iter()
            .filter(|(p, _, _)| p.ends_with(".js"))
            .map(|(_, s, _)| *s)
            .collect::<Vec<_>>()
            .concat()
    }

    /// 骨架里的本地静态引用（href="/…"、src="/…"）必须与 STATIC_ASSETS
    /// 双向一致：加文件忘接线 → 页面静默 404、缺脚本缺样式；删文件留死路由
    /// → 同样 404。拆分后的接线契约由这一条钉住，两头都查。
    #[test]
    fn skeleton_asset_references_and_routes_agree() {
        let mut referenced = Vec::new();
        for needle in ["href=\"/", "src=\"/"] {
            let mut rest: &str = INDEX_HTML;
            while let Some(i) = rest.find(needle) {
                let start = i + needle.len();
                let end = rest[start..].find('"').expect("本地静态引用缺闭合引号");
                referenced.push(format!("/{}", &rest[start..start + end]));
                rest = &rest[start + end..];
            }
        }
        assert!(!referenced.is_empty(), "骨架里没扫到任何本地静态引用");

        for p in &referenced {
            assert!(
                STATIC_ASSETS.iter().any(|(route, ..)| route == p),
                "骨架引用 {p} 没有对应静态路由，浏览器会拿到 404"
            );
        }
        for (route, ..) in STATIC_ASSETS {
            assert!(
                referenced.iter().any(|p| p == route),
                "静态路由 {route} 骨架里没有引用，是永远不会被拉取的死路由"
            );
        }
        // 数量也要相等：同一资源重复引用会掩盖另一条路由的缺失
        assert_eq!(
            referenced.len(),
            STATIC_ASSETS.len(),
            "骨架引用数与路由表条目数不一致"
        );
    }

    /// 与前端 esc() 同一套实体表的镜像，用来跑注射样例回归：
    /// 样例里的尖括号/引号必须全部变成实体，不留可解析成标签的裸 `<`。
    fn esc_mirror(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                '&' => "&amp;".to_string(),
                '<' => "&lt;".to_string(),
                '>' => "&gt;".to_string(),
                '"' => "&quot;".to_string(),
                '\'' => "&#39;".to_string(),
                _ => c.to_string(),
            })
            .collect()
    }

    /// 空点集不能覆盖已查到的点位。
    ///
    /// 后端在手绘、操场、部分预览分支回的是 `checkpoints: []`，空数组在 JS
    /// 里是 truthy，原来 `if (checkpoints) lastCheckpoints = checkpoints`
    /// 会把已经查到的点位整个清空——图上点位全消失、手绘面板「点位 N/M」
    /// 也归零。issue #4 报的「生成路线把点位刷走了」就是这个。
    #[test]
    fn empty_checkpoint_array_must_not_overwrite() {
        assert!(
            console_js().contains("Array.isArray(checkpoints) && checkpoints.length"),
            "drawPreview 必须判非空才更新 lastCheckpoints"
        );
        assert!(
            !console_js().contains("if (checkpoints) lastCheckpoints = checkpoints;"),
            "旧的 truthy 判断还在，空数组照样会覆盖"
        );
    }

    /// 多校区要有可切换的入口。
    ///
    /// 服务端对多校区学校会下发多个围栏，但界面原来只把它们并成一个视野框
    /// （fitBounds），没有任何选择器——用户能用的场地只有锚点碰巧落在的
    /// 那一个。issue #6 的「无法选用其他校区田径场」说的就是这个。
    #[test]
    fn multi_campus_selector_exists() {
        assert!(INDEX_HTML.contains("id=\"fenceSel\""), "缺少校区/场地下拉");
        assert!(
            console_js().contains("function fillFenceSelect"),
            "缺少填充下拉的函数"
        );
        assert!(
            console_js().contains("window.__fences"),
            "下拉的 change 处理要能拿到围栏数组"
        );
        // 选中后必须真的改锚点，否则 select_by_anchor 还是选原来那个
        assert!(
            console_js().contains("setPickLocation(gl, go, false)"),
            "选中校区后要把锚点挪过去"
        );
    }

    #[test]
    fn injection_payload_is_fully_escaped() {
        let payloads = [
            "<img src=x onerror=alert(1)>",
            "\"><svg onload=alert(1)>",
            "a'><script>alert(1)</script>",
        ];
        for p in payloads {
            let out = esc_mirror(p);
            assert!(!out.contains('<'), "残留裸尖括号: {out}");
            assert!(!out.contains('>'), "残留裸尖括号: {out}");
            assert!(!out.contains("<script"));
        }
    }

    #[test]
    fn upstream_sinks_are_wrapped_by_esc() {
        // 转义函数本体必须在页面里，且实体映射齐全
        assert!(console_js().contains("const esc ="));
        for ent in ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;"] {
            assert!(console_js().contains(ent), "esc 实体映射缺失: {ent}");
        }
        // 上游可控字段进入 innerHTML / 弹窗的拼接点必须走 esc()，漏包任一即红
        for sink in [
            "esc(r.fence.name)",
            "esc(c.name)",
            "esc(a.label)",
            "esc(a.username)",
            "esc(a.school",
            "esc(x.error)",
            "esc(r.issueSummary)",
            "esc(r.logs.join('\\n'))",
            "esc(sm.sname",
            "esc(e.message)",
        ] {
            assert!(console_js().contains(sink), "缺少转义包裹: {sink}");
        }
    }

    /// api() 必须带超时与取消：上游挂起时不能让按钮永久停在 busy 态。
    #[test]
    fn api_request_has_timeout_and_abort() {
        assert!(console_js().contains("new AbortController()"));
        assert!(console_js().contains("ctrl.abort()"));
        assert!(console_js().contains("API_TIMEOUT_MS"));
        assert!(console_js().contains("请求超时"));
    }

    /// 写操作必须有防重入：账号行写按钮、禁区保存、picker 登录在请求
    /// 结算前再次点击要被忽略或禁用，不能并发重复提交。
    #[test]
    fn write_actions_guard_reentry() {
        assert!(console_js().contains("accActionBusy"));
        assert!(console_js().contains("busy(btn, true)"));
        assert!(console_js().contains("x.disabled = true"));
    }
}
