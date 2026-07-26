---
name: frontend-design
description: Create distinctive, production-grade frontend interfaces with high design quality. Use this skill when the user asks to build web components, pages, artifacts, posters, or applications (examples include websites, landing pages, dashboards, React components, HTML/CSS layouts, or when styling/beautifying any web UI). Generates creative, polished code and UI design that avoids generic AI aesthetics.
license: Complete terms in LICENSE.txt
---

This skill guides creation of distinctive, production-grade frontend interfaces that avoid generic "AI slop" aesthetics. Implement real working code with exceptional attention to aesthetic details and creative choices.

The user provides frontend requirements: a component, page, application, or interface to build. They may include context about the purpose, audience, or technical constraints.

## Design Thinking

Before coding, understand the context and commit to a BOLD aesthetic direction:
- **Purpose**: What problem does this interface solve? Who uses it?
- **Tone**: Pick an extreme: brutally minimal, maximalist chaos, retro-futuristic, organic/natural, luxury/refined, playful/toy-like, editorial/magazine, brutalist/raw, art deco/geometric, soft/pastel, industrial/utilitarian, etc. There are so many flavors to choose from. Use these for inspiration but design one that is true to the aesthetic direction.
- **Constraints**: Technical requirements (framework, performance, accessibility).
- **Differentiation**: What makes this UNFORGETTABLE? What's the one thing someone will remember?

**CRITICAL**: Choose a clear conceptual direction and execute it with precision. Bold maximalism and refined minimalism both work - the key is intentionality, not intensity.

Then implement working code (HTML/CSS/JS, React, Vue, etc.) that is:
- Production-grade and functional
- Visually striking and memorable
- Cohesive with a clear aesthetic point-of-view
- Meticulously refined in every detail

## Frontend Aesthetics Guidelines

Focus on:
- **Typography**: Choose fonts that are beautiful, unique, and interesting. Avoid generic fonts like Arial and Inter; opt instead for distinctive choices that elevate the frontend's aesthetics; unexpected, characterful font choices. Pair a distinctive display font with a refined body font.
- **Color & Theme**: Commit to a cohesive aesthetic. Use CSS variables for consistency. Dominant colors with sharp accents outperform timid, evenly-distributed palettes.
- **Motion**: Use animations for effects and micro-interactions. Prioritize CSS-only solutions for HTML. Use Motion library for React when available. Focus on high-impact moments: one well-orchestrated page load with staggered reveals (animation-delay) creates more delight than scattered micro-interactions. Use scroll-triggering and hover states that surprise.
- **Spatial Composition**: Unexpected layouts. Asymmetry. Overlap. Diagonal flow. Grid-breaking elements. Generous negative space OR controlled density.
- **Backgrounds & Visual Details**: Create atmosphere and depth rather than defaulting to solid colors. Add contextual effects and textures that match the overall aesthetic. Apply creative forms like gradient meshes, noise textures, geometric patterns, layered transparencies, dramatic shadows, decorative borders, custom cursors, and grain overlays.

NEVER use generic AI-generated aesthetics like overused font families (Inter, Roboto, Arial, system fonts), cliched color schemes (particularly purple gradients on white backgrounds), predictable layouts and component patterns, and cookie-cutter design that lacks context-specific character.

Interpret creatively and make unexpected choices that feel genuinely designed for the context. No design should be the same. Vary between light and dark themes, different fonts, different aesthetics. NEVER converge on common choices (Space Grotesk, for example) across generations.

**IMPORTANT**: Match implementation complexity to the aesthetic vision. Maximalist designs need elaborate code with extensive animations and effects. Minimalist or refined designs need restraint, precision, and careful attention to spacing, typography, and subtle details. Elegance comes from executing the vision well.

Remember: Claude is capable of extraordinary creative work. Don't hold back, show what can truly be created when thinking outside the box and committing fully to a distinctive vision.

---

# 操作指南（来自 frontend-skill）

> 本附录整合自原 `frontend-skill` 技能，提供具体的操作规则。当任务的视觉质量取决于艺术指导、层次、克制、图像和动效（而非组件数量）时，遵循以下规则。

## Working Model

构建前先写下三件事：

- **visual thesis**: 一句话描述情绪、材质和能量
- **content plan**: hero, support, detail, final CTA
- **interaction thesis**: 2-3 个能改变页面感觉的动效想法

每个 section 只做一件事，一个主导视觉想法，一个主要 takeaway 或 action。

## Beautiful Defaults

- 从构图开始，而非组件
- 优先 full-bleed hero 或 full-canvas 视觉锚点
- 让品牌或产品名成为最响亮的文字
- 文案保持可在数秒内扫描完毕
- 优先使用 whitespace、alignment、scale、cropping、contrast，而非添加 chrome
- 限制系统：最多两种字体，默认一种强调色
- 默认 cardless 布局，使用 sections、columns、dividers、lists、media blocks 替代
- 把第一视口当作海报，而非文档

## Landing Pages

默认序列：
1. **Hero**: 品牌/产品、承诺、CTA 和一个主导视觉
2. **Support**: 一个具体的 feature、offer 或 proof point
3. **Detail**: 氛围、workflow、产品深度或故事
4. **Final CTA**: convert、start、visit 或 contact

Hero 规则：
- 只有一个构图
- Full-bleed 图片或主导视觉平面
- Canonical full-bleed 规则：在品牌 landing page 上，hero 必须边缘到边缘，无 inherited page gutters、framed container 或 shared max-width；只约束内部 text/action column
- Brand first, headline second, body third, CTA fourth
- 默认无 hero cards、stat strips、logo clouds、pill soup 或 floating dashboards
- Headlines 在 desktop 上保持约 2-3 行，mobile 上一眼可读
- 保持 text column 窄且锚定在图片的 calm area
- 所有图片上的文字必须保持强对比和清晰 tap target

**检验规则**：如果移除图片后第一视口仍然 work，图片太弱。如果隐藏 nav 后品牌消失，层次太弱。

Viewport budget：
- 如果第一屏包含 sticky/fixed header，header 计入 hero。Combined header + hero content 必须在常见 desktop 和 mobile 尺寸下 fit 在 initial viewport 内
- 使用 `100vh`/`100svh` heroes 时，减去 persistent UI chrome (`calc(100svh - header-height)`) 或 overlay header 而非 stack

## Apps

默认 Linear-style 克制：
- 平静的 surface hierarchy
- 强字体和间距
- 少量颜色
- 密集但可读的信息
- 极简 chrome
- 只在 card 本身就是 interaction 时才用 card

App UI 组织围绕：
- primary workspace
- navigation
- secondary context 或 inspector
- 一个清晰的 accent 用于 action 或 state

避免：
- dashboard-card mosaics
- 每个区域的厚边框
- routine product UI 后的装饰性渐变
- 多个竞争的 accent colors
- 不改善 scanning 的装饰性 icons

如果 panel 可以变成 plain layout 而不丢失意义，移除 card treatment。

## Imagery

Imagery 必须做 narrative work：

- 对 brands、venues、editorial pages 和 lifestyle products，至少使用一张强 real-looking image
- 优先 in-situ photography，而非 abstract gradients 或 fake 3D objects
- 选择或裁剪有稳定 tonal area 的图片用于文字
- 不使用带 embedded signage、logos 或 typographic clutter 的图片
- 不生成带 built-in UI frames、splits、cards 或 panels 的图片
- 如果需要多个 moments，使用多张图片，而非一个 collage

第一视口需要 real visual anchor，decorative texture 不够。

## Copy

- 用产品语言，非设计评论
- 让 headline 承载意义
- Supporting copy 通常是一句短句
- 削减 section 间的重复
- 不在 UI 中包含 prompt language 或 design commentary
- 每个 section 一个职责：explain、prove、deepen 或 convert

如果删除 30% 的 copy 能改善页面，继续删除。

## Utility Copy For Product UI

当工作是 dashboard、app surface、admin tool 或 operational workspace 时，默认 utility copy 而非 marketing copy：

- 优先 orientation、status 和 action，而非 promise、mood 或 brand voice
- 从 working surface 本身开始：KPIs、charts、filters、tables、status 或 task context。除非用户明确要求，不引入 hero section
- Section headings 应说明区域是什么或用户能做什么
- Good: "Selected KPIs", "Plan status", "Search metrics", "Top segments", "Last sync"
- 避免 aspirational hero lines、metaphors、campaign-style language 和 executive-summary banners
- Supporting text 应在一句话内解释 scope、behavior、freshness 或 decision value
- 如果一句话能出现在 homepage hero 或 ad 中，重写直到听起来像 product UI
- 如果 section 不帮助某人 operate、monitor 或 decide，移除
- Litmus check：如果 operator 只扫描 headings、labels 和 numbers，能立即理解页面吗？

## Motion

Use motion to create presence and hierarchy, not noise.

为 visually led work 至少 ship 2-3 个 intentional motions：

- hero 中的一个 entrance sequence
- 一个 scroll-linked、sticky 或 depth effect
- 一个 hover、reveal 或 layout transition 以 sharpen affordance

可用时优先 Framer Motion 用于：
- section reveals
- shared layout transitions
- scroll-linked opacity、translate 或 scale shifts
- sticky storytelling
- carousels that advance narrative
- menus、drawers 和 modal presence effects

Motion 规则：
- 在 quick recording 中 noticeable
- mobile 上 smooth
- fast and restrained
- across the page consistent
- if ornamental only, removed

## Hard Rules

- 默认无 cards
- 默认无 hero cards
- brief 要求 full bleed 时无 boxed 或 center-column hero
- 每 section 不超过一个主导 idea
- 无 section 应需要许多微小 UI devices 来解释自己
- branded pages 上无 headline 应 overpower brand
- 无 filler copy
- 除非 text 在 calm、unified side，无 split-screen hero
- 无 clear reason 时不超过两种 typefaces
- 除非产品已有强系统，不超过一个 accent color

## Reject These Failures

- Generic SaaS card grid 作为第一印象
- Beautiful image with weak brand presence
- Strong headline with no clear action
- Busy imagery behind text
- Sections that repeat the same mood statement
- Carousel with no narrative purpose
- App UI made of stacked cards instead of layout

## Litmus Checks

- 第一屏中品牌或产品是否 unmistakable？
- 是否有一个 strong visual anchor？
- 只扫描 headlines 能否理解页面？
- 每个 section 是否只做一件事？
- Cards 是否真的必要？
- Motion 是否改善 hierarchy 或 atmosphere？
- 移除所有装饰性 shadows 后设计是否仍然 premium？

---

# 附录：预设主题应用（来自 theme-factory）

> 本附录整合自原 `theme-factory` 技能。当需要快速应用预设主题到 artifact（幻灯片、文档、报告、HTML landing pages）时，使用 `theme_factory/themes/` 目录下的预设主题。

## 工具选择决策表

| 场景 | 推荐方式 | 原因 |
|------|---------|------|
| 从零创建独特前端界面 | **frontend-design 主流程**（Design Thinking + Aesthetics Guidelines） | 强调创意和美学方向 |
| 应用预设主题到 artifact | **theme_factory 预设主题**（10 个主题可选） | 快速选择，保证一致性 |
| 自定义新主题 | **theme_factory 自定义流程** | 基于描述生成新主题 |

## 预设主题列表

`theme_factory/themes/` 目录下有 10 个预设主题，每个包含完整配色和字体配对：

1. **Ocean Depths** - 专业沉静的航海主题
2. **Sunset Boulevard** - 温暖鲜艳的日落色彩
3. **Forest Canopy** - 自然稳重的大地色调
4. **Modern Minimalist** - 干净现代的灰阶
5. **Golden Hour** - 富丽温暖的秋日调色板
6. **Arctic Frost** - 冷冽清晰的冬季主题
7. **Desert Rose** - 柔和精致的尘土色调
8. **Tech Innovation** - 大胆现代的科技美学
9. **Botanical Garden** - 清新有机的花园色彩
10. **Midnight Galaxy** - 戏剧化的宇宙深色调

## 主题应用流程

1. **展示主题预览**：向用户展示 `theme_factory/theme-showcase.pdf`（如存在）让用户视觉选择
2. **询问选择**：询问用户要应用哪个主题
3. **等待确认**：获得明确的主题选择确认
4. **应用主题**：读取 `theme_factory/themes/<theme-name>.md`，将指定颜色和字体一致应用到 artifact

## 主题文件结构

每个主题文件包含：
- **Color Palette**: 配色方案（含 hex 码）
- **Typography**: Headers 和 Body Text 的字体配对
- **Best Used For**: 推荐使用场景

示例（Arctic Frost）：
- Ice Blue `#d4e4f7` - 浅背景和高亮
- Steel Blue `#4a6fa5` - 主强调色
- Silver `#c0c0c0` - 金属质感元素
- Crisp White `#fafafa` - 干净背景和文字
- Headers: DejaVu Sans Bold / Body: DejaVu Sans

## 创建自定义主题

如果现有主题都不适合：
1. 基于用户描述选择合适的颜色/字体组合
2. 生成类似命名风格的主题文件
3. 展示给用户审核验证
4. 应用主题到 artifact
