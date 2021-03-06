use std::path::Path;
use std::sync::Arc;

use futures::channel::oneshot::channel as oneshot_channel;
use futures::{stream, SinkExt, StreamExt};

use chromiumoxide_cdp::cdp::browser_protocol;
use chromiumoxide_cdp::cdp::browser_protocol::dom::*;
use chromiumoxide_cdp::cdp::browser_protocol::emulation::{
    MediaFeature, SetEmulatedMediaParams, SetTimezoneOverrideParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::network::{
    Cookie, CookieParam, DeleteCookiesParams, GetCookiesParams, SetCookiesParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::page::*;
use chromiumoxide_cdp::cdp::browser_protocol::performance::{GetMetricsParams, Metric};
use chromiumoxide_cdp::cdp::browser_protocol::target::{SessionId, TargetId};
use chromiumoxide_cdp::cdp::js_protocol;
use chromiumoxide_cdp::cdp::js_protocol::debugger::GetScriptSourceParams;
use chromiumoxide_cdp::cdp::js_protocol::runtime::{EvaluateParams, RemoteObject, ScriptId};
use chromiumoxide_types::*;

use crate::element::Element;
use crate::error::{CdpError, Result};
use crate::handler::target::TargetMessage;
use crate::handler::PageInner;
use crate::layout::Point;
use crate::utils;

#[derive(Debug)]
pub struct Page {
    inner: Arc<PageInner>,
}

impl Page {
    /// Execute a command and return the `Command::Response`
    pub async fn execute<T: Command>(&self, cmd: T) -> Result<CommandResponse<T::Response>> {
        Ok(self.inner.execute(cmd).await?)
    }

    /// This resolves once the navigation finished and the page is loaded.
    ///
    /// This is necessary after an interaction with the page that may trigger a
    /// navigation (`click`, `press_key`) in order to wait until the new browser
    /// page is loaded
    pub async fn wait_for_navigation(&self) -> Result<&Self> {
        self.inner.wait_for_navigation().await?;
        Ok(self)
    }

    /// Navigate directly to the given URL.
    ///
    /// This resolves directly after the requested URL is fully loaded.
    pub async fn goto(&self, params: impl Into<NavigateParams>) -> Result<&Self> {
        let res = self.execute(params.into()).await?;
        if let Some(err) = res.result.error_text {
            return Err(CdpError::ChromeMessage(err));
        }

        Ok(self)
    }

    /// The identifier of the `Target` this page belongs to
    pub fn target_id(&self) -> &TargetId {
        self.inner.target_id()
    }

    /// The identifier of the `Session` target of this page is attached to
    pub fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }

    /// Returns the current url of the page
    pub async fn url(&self) -> Result<Option<String>> {
        let (tx, rx) = oneshot_channel();
        self.inner
            .sender()
            .clone()
            .send(TargetMessage::Url(tx))
            .await?;
        Ok(rx.await?)
    }

    /// Return the main frame of the page
    pub async fn mainframe(&self) -> Result<Option<FrameId>> {
        let (tx, rx) = oneshot_channel();
        self.inner
            .sender()
            .clone()
            .send(TargetMessage::MainFrame(tx))
            .await?;
        Ok(rx.await?)
    }

    /// Allows overriding user agent with the given string.
    pub async fn set_user_agent(
        &self,
        params: impl Into<SetUserAgentOverrideParams>,
    ) -> Result<&Self> {
        self.execute(params.into()).await?;
        Ok(self)
    }

    /// Returns the root DOM node (and optionally the subtree) of the page.
    ///
    /// # Note: This does not return the actual HTML document of the page. To
    /// retrieve the HTML content of the page see `Page::content`.
    pub async fn get_document(&self) -> Result<Node> {
        let resp = self.execute(GetDocumentParams::default()).await?;
        Ok(resp.result.root)
    }

    /// Returns the first element in the document which matches the given CSS
    /// selector.
    ///
    /// Execute a query selector on the document's node.
    pub async fn find_element(&self, selector: impl Into<String>) -> Result<Element> {
        let root = self.get_document().await?.node_id;
        let node_id = self.inner.find_element(selector, root).await?;
        Ok(Element::new(Arc::clone(&self.inner), node_id).await?)
    }

    /// Return all `Element`s in the document that match the given selector
    pub async fn find_elements(&self, selector: impl Into<String>) -> Result<Vec<Element>> {
        let root = self.get_document().await?.node_id;
        let node_ids = self.inner.find_elements(selector, root).await?;
        Ok(Element::from_nodes(&self.inner, &node_ids).await?)
    }

    /// Describes node given its id
    pub async fn describe_node(&self, node_id: NodeId) -> Result<Node> {
        let resp = self
            .execute(
                DescribeNodeParams::builder()
                    .node_id(node_id)
                    .depth(100)
                    .build(),
            )
            .await?;
        Ok(resp.result.node)
    }

    pub async fn close(self) {
        todo!()
    }

    /// Performs a single mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    ///
    /// # Example
    ///
    /// Perform custom click
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::input::{DispatchMouseEventParams, MouseButton, DispatchMouseEventType};
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///      // double click
    ///      let cmd = DispatchMouseEventParams::builder()
    ///             .x(point.x)
    ///             .y(point.y)
    ///             .button(MouseButton::Left)
    ///             .click_count(2);
    ///
    ///         page.move_mouse(point).await?.execute(
    ///             cmd.clone()
    ///                 .r#type(DispatchMouseEventType::MousePressed)
    ///                 .build()
    ///                 .unwrap(),
    ///         )
    ///         .await?;
    ///
    ///         page.execute(
    ///             cmd.r#type(DispatchMouseEventType::MouseReleased)
    ///                 .build()
    ///                 .unwrap(),
    ///         )
    ///         .await?;
    ///
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn click(&self, point: Point) -> Result<&Self> {
        self.inner.click(point).await?;
        Ok(self)
    }

    /// Dispatches a `mousemove` event and moves the mouse to the position of
    /// the `point` where `Point.x` is the horizontal position of the mouse and
    /// `Point.y` the vertical position of the mouse.
    pub async fn move_mouse(&self, point: Point) -> Result<&Self> {
        self.inner.move_mouse(point).await?;
        Ok(self)
    }

    /// Take a screenshot of the current page
    pub async fn screenshot(&self, params: impl Into<CaptureScreenshotParams>) -> Result<Vec<u8>> {
        Ok(self.inner.screenshot(params).await?)
    }

    /// Save a screenshot of the page
    ///
    /// # Example save a png file of a website
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::page::{CaptureScreenshotParams, CaptureScreenshotFormat};
    /// # async fn demo(page: Page) -> Result<()> {
    ///         page.goto("http://example.com")
    ///             .await?
    ///             .save_screenshot(
    ///             CaptureScreenshotParams::builder()
    ///                 .format(CaptureScreenshotFormat::Png)
    ///                 .build(),
    ///             "example.png",
    ///             )
    ///             .await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn save_screenshot(
        &self,
        params: impl Into<CaptureScreenshotParams>,
        output: impl AsRef<Path>,
    ) -> Result<Vec<u8>> {
        let img = self.screenshot(params).await?;
        utils::write(output.as_ref(), &img).await?;
        Ok(img)
    }

    /// Print the current page as pdf.
    ///
    /// See [`PrintToPdfParams`]
    ///
    /// # Note Generating a pdf is currently only supported in Chrome headless.
    pub async fn pdf(&self, params: PrintToPdfParams) -> Result<Vec<u8>> {
        let res = self.execute(params).await?;
        Ok(base64::decode(&res.data)?)
    }

    /// Save the current page as pdf as file to the `output` path and return the
    /// pdf contents.
    ///
    /// # Note Generating a pdf is currently only supported in Chrome headless.
    pub async fn save_pdf(
        &self,
        opts: PrintToPdfParams,
        output: impl AsRef<Path>,
    ) -> Result<Vec<u8>> {
        let pdf = self.pdf(opts).await?;
        utils::write(output.as_ref(), &pdf).await?;
        Ok(pdf)
    }

    /// Brings page to front (activates tab)
    pub async fn bring_to_front(&self) -> Result<&Self> {
        self.execute(BringToFrontParams::default()).await?;
        Ok(self)
    }

    /// Emulates the given media type or media feature for CSS media queries
    pub async fn emulate_media_features(&self, features: Vec<MediaFeature>) -> Result<&Self> {
        self.execute(SetEmulatedMediaParams::builder().features(features).build())
            .await?;
        Ok(self)
    }

    /// Overrides default host system timezone
    pub async fn emulate_timezone(
        &self,
        timezoune_id: impl Into<SetTimezoneOverrideParams>,
    ) -> Result<&Self> {
        self.execute(timezoune_id.into()).await?;
        Ok(self)
    }

    /// Reloads given page
    ///
    /// To reload ignoring cache run:
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::page::ReloadParams;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     page.execute(ReloadParams::builder().ignore_cache(true).build()).await?;
    ///     page.wait_for_navigation().await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn reload(&self) -> Result<&Self> {
        self.execute(ReloadParams::default()).await?;
        Ok(self.wait_for_navigation().await?)
    }

    /// Enables log domain. Enabled by default.
    ///
    /// Sends the entries collected so far to the client by means of the
    /// entryAdded notification.
    ///
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Log#method-enable
    pub async fn enable_log(&self) -> Result<&Self> {
        self.execute(browser_protocol::log::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables log domain
    ///
    /// Prevents further log entries from being reported to the client
    ///
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Log#method-disable
    pub async fn disable_log(&self) -> Result<&Self> {
        self.execute(browser_protocol::log::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables runtime domain. Activated by default.
    pub async fn enable_runtime(&self) -> Result<&Self> {
        self.execute(js_protocol::runtime::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables runtime domain
    pub async fn disable_runtime(&self) -> Result<&Self> {
        self.execute(js_protocol::runtime::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables Debugger. Enabled by default.
    pub async fn enable_debugger(&self) -> Result<&Self> {
        self.execute(js_protocol::debugger::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables Debugger.
    pub async fn disable_debugger(&self) -> Result<&Self> {
        self.execute(js_protocol::debugger::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Activates (focuses) the target.
    pub async fn activate(&self) -> Result<&Self> {
        self.inner.activate().await?;
        Ok(self)
    }

    /// Returns all cookies that match the tab's current URL.
    pub async fn get_cookies(&self) -> Result<Vec<Cookie>> {
        Ok(self
            .execute(GetCookiesParams::default())
            .await?
            .result
            .cookies)
    }

    /// Set a single cookie
    ///
    /// This fails if the cookie's url or if not provided, the page's url is
    /// `about:blank` or a `data:` url.
    ///
    /// # Example
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::network::CookieParam;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     page.set_cookie(CookieParam::new("Cookie-name", "Cookie-value")).await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn set_cookie(&self, cookie: impl Into<CookieParam>) -> Result<&Self> {
        let mut cookie = cookie.into();
        if let Some(url) = cookie.url.as_ref() {
            validate_cookie_url(url)?;
        } else {
            let url = self
                .url()
                .await?
                .ok_or_else(|| CdpError::msg("Page url not found"))?;
            validate_cookie_url(&url)?;
            if url.starts_with("http") {
                cookie.url = Some(url);
            }
        }
        self.execute(DeleteCookiesParams::from_cookie(&cookie))
            .await?;
        self.execute(SetCookiesParams::new(vec![cookie])).await?;
        Ok(self)
    }

    /// Set all the cookies
    pub async fn set_cookies(&self, mut cookies: Vec<CookieParam>) -> Result<&Self> {
        let url = self
            .url()
            .await?
            .ok_or_else(|| CdpError::msg("Page url not found"))?;
        let is_http = url.starts_with("http");
        if !is_http {
            validate_cookie_url(&url)?;
        }

        for cookie in &mut cookies {
            if let Some(url) = cookie.url.as_ref() {
                validate_cookie_url(url)?;
            } else {
                if is_http {
                    cookie.url = Some(url.clone());
                }
            }
        }
        self.delete_cookies_unchecked(cookies.iter().map(DeleteCookiesParams::from_cookie))
            .await?;

        self.execute(SetCookiesParams::new(cookies)).await?;
        Ok(self)
    }

    /// Delete a single cookie
    pub async fn delete_cookie(&self, cookie: impl Into<DeleteCookiesParams>) -> Result<&Self> {
        let mut cookie = cookie.into();
        if cookie.url.is_none() {
            let url = self
                .url()
                .await?
                .ok_or_else(|| CdpError::msg("Page url not found"))?;
            if url.starts_with("http") {
                cookie.url = Some(url);
            }
        }
        self.execute(cookie).await?;
        Ok(self)
    }

    /// Delete all the cookies
    pub async fn delete_cookies(&self, mut cookies: Vec<DeleteCookiesParams>) -> Result<&Self> {
        let mut url: Option<(String, bool)> = None;
        for cookie in &mut cookies {
            if cookie.url.is_none() {
                if let Some((url, is_http)) = url.as_ref() {
                    if *is_http {
                        cookie.url = Some(url.clone())
                    }
                } else {
                    let page_url = self
                        .url()
                        .await?
                        .ok_or_else(|| CdpError::msg("Page url not found"))?;
                    let is_http = page_url.starts_with("http");
                    if is_http {
                        cookie.url = Some(page_url.clone())
                    }
                    url = Some((page_url, is_http));
                }
            }
        }
        self.delete_cookies_unchecked(cookies.into_iter()).await?;
        Ok(self)
    }

    /// Convenience method that prevents another channel roundtrip to get the
    /// url and validate it
    async fn delete_cookies_unchecked(
        &self,
        cookies: impl Iterator<Item = DeleteCookiesParams>,
    ) -> Result<&Self> {
        // NOTE: the buffer size is arbitrary
        let mut cmds = stream::iter(cookies.into_iter().map(|cookie| self.execute(cookie)))
            .buffer_unordered(5);
        while let Some(resp) = cmds.next().await {
            resp?;
        }
        Ok(self)
    }

    /// Returns the title of the document.
    pub async fn get_title(&self) -> Result<Option<String>> {
        let remote_object = self.evaluate("document.title").await?;
        let title: String = serde_json::from_value(
            remote_object
                .value
                .ok_or_else(|| CdpError::msg("No title found"))?,
        )?;
        if title.is_empty() {
            Ok(None)
        } else {
            Ok(Some(title))
        }
    }

    /// Retrieve current values of run-time metrics.
    pub async fn metrics(&self) -> Result<Vec<Metric>> {
        Ok(self
            .execute(GetMetricsParams::default())
            .await?
            .result
            .metrics)
    }

    /// Returns metrics relating to the layout of the page
    pub async fn layout_metrics(&self) -> Result<GetLayoutMetricsReturns> {
        Ok(self.inner.layout_metrics().await?)
    }

    /// Evaluates expression on global object.
    pub async fn evaluate(&self, evaluate: impl Into<EvaluateParams>) -> Result<RemoteObject> {
        Ok(self.execute(evaluate.into()).await?.result.result)
    }

    /// Evaluates given script in every frame upon creation (before loading
    /// frame's scripts)
    pub async fn evaluate_on_new_document(
        &self,
        script: impl Into<AddScriptToEvaluateOnNewDocumentParams>,
    ) -> Result<ScriptIdentifier> {
        Ok(self.execute(script.into()).await?.result.identifier)
    }

    pub async fn set_content(&self, html: impl AsRef<str>) -> Result<&Self> {
        let js = format!(
            "(html) => {{
      document.open();
      document.write(html);
      document.close();
    }}, {})",
            html.as_ref()
        );
        self.evaluate(js).await?;
        // relying that document.open() will reset frame lifecycle with "init"
        // lifecycle event. @see https://crrev.com/608658
        Ok(self.wait_for_navigation().await?)
    }

    /// Returns the HTML content of the page
    pub async fn content(&self) -> Result<String> {
        let resp = self
            .evaluate(
                "{
          let retVal = '';
          if (document.doctype) {
            retVal = new XMLSerializer().serializeToString(document.doctype);
          }
          if (document.documentElement) {
            retVal += document.documentElement.outerHTML;
          }
          retVal
      }
      ",
            )
            .await?;
        let value = resp.value.ok_or(CdpError::NotFound)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Returns source for the script with given id.
    ///
    /// Debugger must be enabled.
    pub async fn get_script_source(&self, script_id: impl Into<String>) -> Result<String> {
        Ok(self
            .execute(GetScriptSourceParams::new(ScriptId::from(script_id.into())))
            .await?
            .result
            .script_source)
    }
}

impl From<Arc<PageInner>> for Page {
    fn from(inner: Arc<PageInner>) -> Self {
        Self { inner }
    }
}

fn validate_cookie_url(url: &str) -> Result<()> {
    if url.starts_with("data:") {
        Err(CdpError::msg("Data URL page can not have cookie"))
    } else if url != "about:blank" {
        Err(CdpError::msg("Blank page can not have cookie"))
    } else {
        Ok(())
    }
}
