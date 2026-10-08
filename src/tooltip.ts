/**
 * 自己的悬停提示（tooltip）—— 样式照 VS Code 的 `.workbench-hover` 抄的。
 *
 * ★★ 为什么不能直接给 `title` 属性加样式：**原生 tooltip 的样式没有任何办法改**。
 *   它是 WebView2 / 系统画的那个小方框，CSS 碰不到，也不看我们的 `color-scheme` ——
 *   深色主题下会弹出一个浅色框，和整个界面格格不入。
 *   VS Code 的悬停提示**根本不是原生的**，是它自己画的一层浮层
 *   （`.workbench-hover`）。所以想「跟 VS Code 一样」，就只能也自己画一层。
 *
 * ★ 做法是**全局接管 `title` 属性**，而不是给 47 处 `:title` 各挂一个指令：
 *   · 启动时把页面上所有 `[title]` 搬成 `data-tooltip`，**并删掉 `title`** ——
 *     不删的话原生那个还会自己弹出来，变成两个框
 *   · `MutationObserver` 盯着后续新增 / 变化的元素 ——
 *     标签页、文件树、搜索结果这些都是动态出来的，静态扫一遍远远不够
 *   · 好处：模板里 47 处 `:title="…"` **一行都不用改**；
 *     做成 `v-tooltip` 指令的话，以后每加一处都要记得加，迟早会漏
 *     （而漏掉的表现是「就那一个地方不是 VS Code 的样子」，不报错）
 *
 * ★ 样式不是自己拍的，是从 VS Code 里量出来的：
 *   · `.workbench-hover`：`font-size: 13px` / `line-height: 19px` /
 *     `border-radius: 5px` / `max-width: 700px` / `border: 1px solid`
 *   · `.workbench-hover.compact .hover-contents { padding: 2px 8px }`
 *     —— `compact` 就是「一行纯文字」那种，正是我们这种
 *   · 阴影：`--vscode-shadow-lg` = `0 0 12px rgba(0,0,0,.14)`
 *   · 颜色：`editorHoverWidget.background` 的**默认值就是** `editorWidget.background`
 *     （JS 里写着 `editorHoverWidget.background = editorWidget.background`），
 *     而那个我们已经映射成 `--color-menu-bg` 了 ⇒ **一个主题变量都不用新增**
 *   · `editorHoverWidget.border` 的默认值是「前景色 20% 透明」
 *     （`transparent(foreground, .2)`）⇒ 用 `color-mix` 现算，
 *     比写死两套色值可靠 —— 后者会和主题跑偏，而且不报错
 *
 * ★ 两个行为细节也是照抄 VS Code 的：
 *   · 延迟取 `workbench.hover.delay`：**非 macOS 上默认 500ms**
 *   · **刚藏起来不到 200ms 又悬停到别的元素 → 立刻显示**
 *     （`get delay()` 里就是这么写的）。没有这一条，
 *     在列表里一行行扫过去会一顿一顿的
 *   ⚠ 只有「真的显示过」才算数：快速扫过去（一次都没来得及显示）不给这个加速，
 *     否则鼠标划过一堆按钮会一路闪现提示条
 */

/** `workbench.hover.delay` 在非 macOS 上的默认值 */
const HOVER_DELAY = 500;

/** 刚藏起来多久之内再悬停，就直接显示（VS Code 里是 200ms） */
const INSTANT_WINDOW = 200;

/** 提示条离光标多远。比原生那个紧一点，和 VS Code 的手感接近 */
const CURSOR_GAP_X = 12;
const CURSOR_GAP_Y = 16;

/** 离视口边缘至少留这么多，免得框贴着边 */
const VIEWPORT_MARGIN = 4;

const TOOLTIP_CLASS = "toocode-tooltip";

let tip: HTMLElement | null = null;
let owner: Element | null = null;
let showTimer: ReturnType<typeof setTimeout> | null = null;
let observer: MutationObserver | null = null;

/**
 * 光标**最新**的位置。
 *
 * ★★ 为什么不能直接用 `mouseover` 事件里的 `clientX/clientY`：
 *   那是「**刚进入这个元素那一刻**」的坐标，而提示条是 500ms 之后才弹的 ——
 *   在一个宽元素里（长标签、状态栏那串完整路径、树的一行）横向滑一段再停下，
 *   弹出来的框还钉在你「进来」的那个点上，离光标老远。
 *   实测：从 x=352 进去、滑到 x=728 停下，框出现在 364。
 *   ★ 而**在同一个元素里移动是不会再触发 mouseover 的** ——
 *     所以光靠 mouseover 根本拿不到后面的位置，必须自己听着 mousemove
 */
const pointer = { x: 0, y: 0 };

/** 上一次「藏起来」的时刻。用来实现那个「紧接着悬停就立刻显示」 */
let hiddenAt = 0;

const disposers: Array<() => void> = [];

/**
 * 标记：这个元素的 `aria-label` 是**我们补的**，值 = 上一次补进去的文本。
 *
 * ★ 为什么要把「上次补的那个值」存下来：靠它区分「这标记是我们写的、作者
 *   没动过」和「作者后来自己改了 aria-label」（那就该让位，不再接管）
 */
const A11Y_FROM_TITLE = "data-a11y-from-title";

/**
 * 把元素上的 `title` 搬成 `data-tooltip`。
 *
 * ★ 必须**删掉** `title`：留着的话原生那个提示还会自己弹出来，
 *   于是光标停一会儿会看到两个框叠在一起
 *
 * ★★ 这个函数会被**反复调用**（MutationObserver 盯着 title 的变化），
 *   所以每种情况都要能重复走：`title` 变了就重新搬一遍
 */
function capture(element: Element): void {
  const text = element.getAttribute("title");

  // ★ `title` 没了 / 变成空串 = 「这里不要提示了」——必须把 data-tooltip 一起去掉。
  //   留着的话会显示**上一次**那句（过期的信息比没有信息更糟）。
  //   Vue 把 `:title="cond ? 'a' : ''"` 改成空串、或者直接不要了，都会走到这
  // ⚠ 不会因此无限循环：观察者那句 `getAttribute("title") !== null` 挡住了
  //   「我们自己 remove 掉 title」那一轮
  if (text === null || text.trim() === "") {
    element.removeAttribute("data-tooltip");
    const own = element.getAttribute(A11Y_FROM_TITLE);
    if (own !== null && element.getAttribute("aria-label") === own) {
      element.removeAttribute("aria-label");
      element.removeAttribute(A11Y_FROM_TITLE);
    }
    element.removeAttribute("title");
    return;
  }

  element.setAttribute("data-tooltip", text);

  // ⚠ `title` 在无障碍里是**最后兜底的名称来源**：一个只有图标的按钮
  //   （窗口那三个按钮、活动栏图标）靠的就是它。搬走之后要把这份信息补回去 ——
  //   但只补**真正光秃秃**的那种（没有 aria-label、也没有可见文字）：
  //   `aria-label` 的优先级比可见文字高，随便加会**把名称改得更糟**
  const patched = element.getAttribute(A11Y_FROM_TITLE);

  if (patched !== null) {
    // ★★ 我们补过 ⇒ **跟着 title 一起更新**。
    //   漏了这一步的后果很隐蔽：动态 title（「聊天记录（2）」这种）改了之后，
    //   屏幕阅读器会一直读到**第一次补进去的那个数字** ——
    //   界面上看不出任何不对，只有用读屏的人会发现名字和实际不符
    // ⚠ 但只在「还是我们上次写的那个值」时才接管：作者若自己改过
    //   aria-label，就不该被我们覆盖回去
    if (element.getAttribute("aria-label") === patched) {
      element.setAttribute("aria-label", text);
      element.setAttribute(A11Y_FROM_TITLE, text);
    }
  } else if (
    element.getAttribute("aria-label") === null &&
    (element.textContent ?? "").trim() === ""
  ) {
    element.setAttribute("aria-label", text);
    element.setAttribute(A11Y_FROM_TITLE, text);
  }

  element.removeAttribute("title");
}

function captureAll(root: Element): void {
  if (root.hasAttribute("title")) capture(root);
  for (const element of root.querySelectorAll("[title]")) capture(element);
}

/** 从事件目标往上找最近的带提示的元素 */
function ownerOf(target: EventTarget | null): Element | null {
  return target instanceof Element ? target.closest("[data-tooltip]") : null;
}

function cancelTimer(): void {
  if (showTimer === null) return;
  clearTimeout(showTimer);
  showTimer = null;
}

function hide(): void {
  cancelTimer();
  if (tip !== null && tip.style.display !== "none") {
    tip.style.display = "none";
    // ★ 只有「真的显示过」才记时刻 —— 见文件头那条说明
    hiddenAt = Date.now();
  }
  owner = null;
}

function show(element: Element): void {
  if (tip === null) return;

  const text = element.getAttribute("data-tooltip") ?? "";
  if (text === "") return;

  tip.textContent = text;

  // ★ 先「隐身量一次」，才知道它占多大 —— 不量就没法算该往哪边翻。
  //   直接显示的坏处是第一次会闪一下（先出现在右下角、再跳到左边）
  tip.style.visibility = "hidden";
  tip.style.display = "block";
  const { width, height } = tip.getBoundingClientRect();

  // 默认放在光标右下方
  let left = pointer.x + CURSOR_GAP_X;
  // ★★ 右边放不下就翻到光标**左边**，而不是「贴着窗口右边缘」——
  //    后者对一个宽提示条（一长串路径能到 700px）来说是错的：
  //    它会把框横跨到**光标上方**，看着像「弹到老远的地方」
  if (left + width > window.innerWidth - VIEWPORT_MARGIN) {
    left = pointer.x - width - CURSOR_GAP_X;
  }

  // 下面放不下就翻到光标上面
  let top = pointer.y + CURSOR_GAP_Y;
  if (top + height > window.innerHeight - VIEWPORT_MARGIN) {
    top = pointer.y - height - CURSOR_GAP_Y;
  }

  // 兼底：夹进窗口。
  // ⚠ 两个 Math.max 不能合 —— 窗口比提示条还窄时，
  //   `innerWidth - width - MARGIN` 会是个负数，直接用会把框推到视口外面
  left = Math.min(
    Math.max(left, VIEWPORT_MARGIN),
    Math.max(VIEWPORT_MARGIN, window.innerWidth - width - VIEWPORT_MARGIN),
  );
  top = Math.min(
    Math.max(top, VIEWPORT_MARGIN),
    Math.max(VIEWPORT_MARGIN, window.innerHeight - height - VIEWPORT_MARGIN),
  );

  tip.style.left = `${Math.round(left)}px`;
  tip.style.top = `${Math.round(top)}px`;
  tip.style.visibility = "visible";
}

/**
 * 装上去。重复调用是安全的（HMR 时模块会被重新求值，
 * 而模块级的变量那时已经归零了 ⇒ 所以判据看 **DOM 里有没有**，不看变量）
 */
export function installTooltips(): void {
  if (document.querySelector(`.${TOOLTIP_CLASS}`) !== null) return;

  tip = document.createElement("div");
  tip.className = TOOLTIP_CLASS;
  tip.setAttribute("role", "tooltip");
  tip.style.display = "none";
  document.body.appendChild(tip);

  // 首屏已有的那些
  captureAll(document.body);

  const onOver = (event: MouseEvent) => {
    // 兜底：万一只发过一次 mouseover、没有 mousemove（合成事件、笔 / 手指），
    // 至少还能用这一份坐标
    if (event.clientX !== 0 || event.clientY !== 0) {
      pointer.x = event.clientX;
      pointer.y = event.clientY;
    }

    const next = ownerOf(event.target);
    // 还在同一个元素内部转（比如从文字移到它旁边的图标），别打断
    if (next === owner) return;

    hide();
    if (next === null) return;

    owner = next;
    const delay = Date.now() - hiddenAt < INSTANT_WINDOW ? 0 : HOVER_DELAY;
    showTimer = setTimeout(() => {
      showTimer = null;
      // 等这 500ms 里鼠标可能已经走了 —— 走的时候 owner 会被清掉
      if (owner === next) show(next);
    }, delay);
  };

  const onMove = (event: MouseEvent) => {
    pointer.x = event.clientX;
    pointer.y = event.clientY;
  };

  const onOut = (event: MouseEvent) => {
    if (owner === null) return;
    // ⚠ 移到自己的子元素上也会触发 mouseout，那种不算「离开」
    const to = event.relatedTarget;
    if (to instanceof Node && owner.contains(to)) return;
    hide();
  };

  const onDismiss = () => hide();
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") hide();
  };

  // 捕获阶段：这些都是「别处也会处理」的事件，冒泡阶段挂可能轮不到我们
  document.addEventListener("mouseover", onOver, true);
  document.addEventListener("mouseout", onOut, true);
  // ★ 光标最新位置就靠它（见 pointer 的说明）—— 频率很高，但只是两个赋值
  document.addEventListener("mousemove", onMove, true);
  document.addEventListener("mousedown", onDismiss, true);
  document.addEventListener("wheel", onDismiss, true);
  document.addEventListener("keydown", onKeyDown, true);
  // ★ 滚动事件**不冒泡**，只能靠捕获才听得到内部容器的滚动
  document.addEventListener("scroll", onDismiss, true);
  window.addEventListener("blur", onDismiss);

  disposers.push(
    () => document.removeEventListener("mouseover", onOver, true),
    () => document.removeEventListener("mouseout", onOut, true),
    () => document.removeEventListener("mousemove", onMove, true),
    () => document.removeEventListener("mousedown", onDismiss, true),
    () => document.removeEventListener("wheel", onDismiss, true),
    () => document.removeEventListener("keydown", onKeyDown, true),
    () => document.removeEventListener("scroll", onDismiss, true),
    () => window.removeEventListener("blur", onDismiss),
  );

  observer = new MutationObserver((records) => {
    for (const record of records) {
      if (record.type === "attributes") {
        // 只在它**真的有** title 的时候搬 —— 我们自己 remove 掉的那次
        // 也会把这个观察者吵醒，那时 getAttribute 已经是 null 了。
        // ⚠ 少了这一句就是无限循环
        const element = record.target as Element;
        if (element.getAttribute("title") !== null) capture(element);
        continue;
      }
      for (const node of record.addedNodes) {
        if (node instanceof Element) captureAll(node);
      }
    }
  });
  observer.observe(document.body, {
    childList: true,
    subtree: true,
    attributes: true,
    // ★ 只盯 title 这一个属性：不然 Vue 每改一次 class / style 都要喊我们一次
    attributeFilter: ["title"],
  });
}

/**
 * 拆掉（目前只有测试和 HMR 会用到）。
 *
 * ★ 不去把 `data-tooltip` 还原成 `title`：那要遍历整个 DOM，
 *   而且真要用得上的场合（页面重载）本来就会重新建一遍 DOM
 */
export function disposeTooltips(): void {
  for (const off of disposers) off();
  disposers.length = 0;

  observer?.disconnect();
  observer = null;

  cancelTimer();
  tip?.remove();
  tip = null;
  owner = null;
}
