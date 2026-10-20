# -*- coding: utf-8 -*-
"""把酷安发布稿（markdown）转成排版精美的 Word 文档。

为什么用脚本而不是手工排版：
  文章以后还可能改。内容只有一份真源（markdown），
  改完重跑本脚本即可，不会出现"文档和文章不一致"。

⚠️ 只处理本文用到的那部分 markdown：标题、段落、粗体/斜体/行内代码、
   裸链接、引用块（渲染成提示框）、表格、有序/无序列表、分隔线。
   它不是通用 md→docx 转换器，也不打算是。

用法：
  python tools/docx/make-docx.py

依赖：
  pip install python-docx
"""

import re
import sys
from pathlib import Path

from docx import Document
from docx.enum.section import WD_SECTION
from docx.enum.table import WD_TABLE_ALIGNMENT
from docx.enum.text import WD_ALIGN_PARAGRAPH, WD_LINE_SPACING
from docx.opc.constants import RELATIONSHIP_TYPE as RT
from docx.oxml import OxmlElement
from docx.oxml.ns import qn
from docx.shared import Cm, Pt, RGBColor

# ============================================================
#  设计常量（改这里就能整体调风格）
# ============================================================

FONT = "微软雅黑"          # 正文与标题统一用雅黑，现代、干净
FONT_MONO = "Consolas"     # 等宽（行内代码）

C_TITLE = "0F4C81"        # 主色：经典蓝
C_H1 = "0F4C81"
C_H2 = "1B6CA8"
C_BODY = "202124"
C_MUTED = "5F6368"
C_TIP_BG = "EEF6FF"        # 提示框底色
C_TIP_BAR = "3B82F6"       # 提示框左侧竖条
C_TIP_TEXT = "1A4E8A"
C_CODE_BG = "F1F3F5"
C_RULE = "D8DEE4"
C_TABLE_HEAD = "0F4C81"
C_TABLE_ALT = "F5F9FD"
C_TABLE_LINE = "CFDAE6"

BODY_PT = 10.5

# ── 行距用**固定值**而不是倍数 ──
#
# 🔴 踩过的坑：倍数行距（MULTIPLE 1.7）在**含粗体的段落**里会出现
#    "这一行比那一行高"的现象 —— Word 按每行**实际用到的字体**算单倍行高，
#    而粗体 run 解析到的物理字体与常规不同，于是同一段里行距忽大忽小，
#    看起来像排版坏了。
#
# 固定值（EXACTLY）让每行高度**完全相同**，与字体无关。
# 10.5pt 的字配 17.5pt 行高很宽松，不会切字。
LINE_BODY = 17.5
LINE_TIP = 16.0     # 提示框正文 10pt
LINE_H1 = 21.0      # 一号标题 15pt
LINE_H2 = 17.0      # 二号标题 12pt
LINE_TITLE = 28.0   # 主标题 20pt
LINE_TITLE2 = 21.0  # 副标题行 15pt
LINE_SMALL = 15.0   # 表格单元格 / 小字
LINE_SPACER = 7.0   # 留白段

# ============================================================
#  输入 / 输出
# ============================================================
#
# ⚠️ 路径以**仓库根目录**为基准（脚本在 tools/docx/ 下，上溯两级）。
#    这样从任何目录调用都成立，不依赖当前工作目录。
_ROOT = Path(__file__).resolve().parents[2]

SRC = _ROOT / "dist" / "酷安发布稿.md"
OUT = _ROOT / "dist" / "澎湃充控 HyperPower · 酷安发布稿.docx"

# 表情符号：Word 里 微软雅黑 没有这些字形，会渲染成空白或豆腐块
# （markdown 里保留，酷安那边能正常显示；只是转 Word 时去掉）
EMOJI = re.compile(
    "[\U0001F300-\U0001FAFF\U00002600-\U000027BF\U0001F000-\U0001F2FF"
    "\U0000FE00-\U0000FE0F\U00002B00-\U00002BFF]+"
)


def strip_emoji(s: str) -> str:
    """去掉表情符号与变体选择符，并清掉因此产生的多余空格。"""
    return re.sub(r"^\s+", "", EMOJI.sub("", s)).rstrip()


def smart_quotes(s: str) -> str:
    """把英文直引号成对换成中文弯引号。

    直引号是 ASCII 字符，Word 允许在它前后断行，于是常见
    `……它"什么` / `都不做"……` 这种把引号甩到下一行开头的难看断法。
    中文弯引号与汉字同属 CJK 类，断行规则正常。
    """
    out = []
    open_q = True
    for ch in s:
        if ch == '"':
            out.append("\u201c" if open_q else "\u201d")
            open_q = not open_q
        elif ch == "'":
            out.append("\u2018" if open_q else "\u2019")
            open_q = not open_q
        else:
            out.append(ch)
    return "".join(out)


# ============================================================
#  底层工具：字体 / 底纹 / 边框 / 超链接
# ============================================================

def set_run_font(run, name=FONT, size=BODY_PT, bold=None, color=None, mono=False):
    """设置一个 run 的字体（含东亚字体，这是中文排版的关键）。"""
    f = FONT_MONO if mono else name
    run.font.name = f
    run.font.size = Pt(size)
    if bold is not None:
        run.font.bold = bold
    if color:
        run.font.color.rgb = RGBColor.from_string(color)
    # ⚠️ 必须显式设 w:eastAsia，否则中文会退回默认宋体
    rpr = run._element.get_or_add_rPr()
    rfonts = rpr.find(qn("w:rFonts"))
    if rfonts is None:
        rfonts = OxmlElement("w:rFonts")
        rpr.append(rfonts)
    rfonts.set(qn("w:ascii"), f)
    rfonts.set(qn("w:hAnsi"), f)
    rfonts.set(qn("w:eastAsia"), f)


def shade_run(run, fill):
    """给 run 加底纹（行内代码用）。"""
    rpr = run._element.get_or_add_rPr()
    shd = OxmlElement("w:shd")
    shd.set(qn("w:val"), "clear")
    shd.set(qn("w:color"), "auto")
    shd.set(qn("w:fill"), fill)
    rpr.append(shd)


def shade_paragraph(p, fill):
    """给整段加底纹（提示框用）。"""
    ppr = p._p.get_or_add_pPr()
    shd = OxmlElement("w:shd")
    shd.set(qn("w:val"), "clear")
    shd.set(qn("w:color"), "auto")
    shd.set(qn("w:fill"), fill)
    ppr.append(shd)


def para_borders(p, left=None, bottom=None, left_size=18, bottom_size=6):
    """给段落加边框。left 用来做提示框的竖条，bottom 用来做标题下划线/分隔线。"""
    ppr = p._p.get_or_add_pPr()
    pbdr = OxmlElement("w:pBdr")
    if left:
        e = OxmlElement("w:left")
        e.set(qn("w:val"), "single")
        e.set(qn("w:sz"), str(left_size))
        e.set(qn("w:space"), "8")
        e.set(qn("w:color"), left)
        pbdr.append(e)
    if bottom:
        e = OxmlElement("w:bottom")
        e.set(qn("w:val"), "single")
        e.set(qn("w:sz"), str(bottom_size))
        e.set(qn("w:space"), "4")
        e.set(qn("w:color"), bottom)
        pbdr.append(e)
    ppr.append(pbdr)


def set_spacing(p, before=0, after=8, line=LINE_BODY, keep_next=False):
    """固定行距 + 段前段后。

    `keep_next` 用于标题：避免标题孤零零落在页面最底部（下一段被推到下一页）。
    """
    pf = p.paragraph_format
    pf.space_before = Pt(before)
    pf.space_after = Pt(after)
    pf.line_spacing_rule = WD_LINE_SPACING.EXACTLY
    pf.line_spacing = Pt(line)
    if keep_next:
        pf.keep_with_next = True


def add_hyperlink(paragraph, url, text, size=BODY_PT):
    """插入真正可点击的超链接（python-docx 原生不支持）。"""
    part = paragraph.part
    r_id = part.relate_to(url, RT.HYPERLINK, is_external=True)

    link = OxmlElement("w:hyperlink")
    link.set(qn("r:id"), r_id)

    run = OxmlElement("w:r")
    rpr = OxmlElement("w:rPr")

    rfonts = OxmlElement("w:rFonts")
    rfonts.set(qn("w:ascii"), FONT_MONO)
    rfonts.set(qn("w:hAnsi"), FONT_MONO)
    rfonts.set(qn("w:eastAsia"), FONT)
    rpr.append(rfonts)

    c = OxmlElement("w:color")
    c.set(qn("w:val"), C_H2)
    rpr.append(c)

    u = OxmlElement("w:u")
    u.set(qn("w:val"), "single")
    rpr.append(u)

    sz = OxmlElement("w:sz")
    sz.set(qn("w:val"), str(int(size * 2)))
    rpr.append(sz)

    run.append(rpr)
    t = OxmlElement("w:t")
    t.set(qn("xml:space"), "preserve")
    t.text = text
    run.append(t)
    link.append(run)
    paragraph._p.append(link)


def add_page_number_footer(section):
    """页脚：左侧模块名，右侧"第 N 页"。"""
    p = section.footer.paragraphs[0]
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    r = p.add_run("澎湃充控 HyperPower · 亮屏也能快充     ")
    set_run_font(r, size=8.5, color=C_MUTED)

    r2 = p.add_run("第 ")
    set_run_font(r2, size=8.5, color=C_MUTED)
    fld1 = OxmlElement("w:fldChar")
    fld1.set(qn("w:fldCharType"), "begin")
    instr = OxmlElement("w:instrText")
    instr.set(qn("xml:space"), "preserve")
    instr.text = "PAGE"
    fld2 = OxmlElement("w:fldChar")
    fld2.set(qn("w:fldCharType"), "end")
    rr = p.add_run()
    set_run_font(rr, size=8.5, color=C_MUTED)
    rr._r.append(fld1)
    rr._r.append(instr)
    rr._r.append(fld2)
    r3 = p.add_run(" 页")
    set_run_font(r3, size=8.5, color=C_MUTED)


# ============================================================
#  段落级构建
# ============================================================

def add_title(doc, text):
    """主标题。

    标题里有 `——` 时断成两行：一行是名字、一行是卖点。
    否则 Word 会按宽度硬断，出现"亮屏也能快 / 充，温控交给你自己"这种难看的断法。
    """
    if " —— " in text:
        head, tail = text.split(" —— ", 1)
    else:
        head, tail = text, None

    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    set_spacing(p, before=0, after=4, line=LINE_TITLE, keep_next=True)
    r = p.add_run(smart_quotes(head))
    set_run_font(r, size=20, bold=True, color=C_TITLE)

    if tail:
        p2 = doc.add_paragraph()
        p2.alignment = WD_ALIGN_PARAGRAPH.CENTER
        set_spacing(p2, before=0, after=6, line=LINE_TITLE2)
        r2 = p2.add_run(smart_quotes(tail))
        set_run_font(r2, size=15, bold=True, color=C_H2)
    return p


def add_subtitle(doc, text):
    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    set_spacing(p, before=0, after=16, line=LINE_SMALL)
    r = p.add_run(strip_emoji(text))
    set_run_font(r, size=10, color=C_MUTED)
    para_borders(p, bottom=C_RULE, bottom_size=6)
    return p


def add_h1(doc, text):
    p = doc.add_paragraph()
    set_spacing(p, before=12, after=9, line=LINE_H1, keep_next=True)
    r = p.add_run(smart_quotes(strip_emoji(text)))
    set_run_font(r, size=15, bold=True, color=C_H1)
    para_borders(p, bottom=C_RULE, bottom_size=6)
    return p


def add_h2(doc, text):
    p = doc.add_paragraph()
    set_spacing(p, before=11, after=5, line=LINE_H2, keep_next=True)
    r = p.add_run(smart_quotes(strip_emoji(text)))
    set_run_font(r, size=12, bold=True, color=C_H2)
    return p


def add_body(doc, text, size=BODY_PT, color=C_BODY, after=8, indent=0):
    """正文段落，支持 **粗体**、`代码`、裸 URL。"""
    p = doc.add_paragraph()
    set_spacing(p, after=after)
    if indent:
        p.paragraph_format.left_indent = Cm(indent)
    emit_inline(p, text, size=size, color=color)
    return p


def emit_inline(p, text, size=BODY_PT, color=C_BODY):
    """把一行 markdown 文本拆成若干 run。

    ⚠️ 正则里 `**粗体**` 必须排在 `*斜体*` **前面** ——
    交替分支按顺序匹配，反过来会把 `**` 拆成两个斜体标记，
    星号就会原样漏到正文里。
    """
    text = strip_emoji(text)
    pattern = re.compile(r"(\*\*[^*]+\*\*|\*[^*]+\*|`[^`]+`|https?://\S+)")
    for piece in pattern.split(text):
        if not piece:
            continue
        if piece.startswith("**") and piece.endswith("**"):
            r = p.add_run(smart_quotes(piece[2:-2]))
            set_run_font(r, size=size, bold=True, color=C_TITLE)
        elif piece.startswith("*") and piece.endswith("*"):
            # 斜体：用于免责声明这类"边注"，压低存在感
            r = p.add_run(smart_quotes(piece[1:-1]))
            set_run_font(r, size=size - 0.5, color=C_MUTED)
            r.font.italic = True
        elif piece.startswith("`") and piece.endswith("`"):
            r = p.add_run(piece[1:-1])
            set_run_font(r, size=size - 0.5, color="B32D2D", mono=True)
            shade_run(r, C_CODE_BG)
        elif piece.startswith("http"):
            add_hyperlink(p, piece, piece, size=size - 0.5)
        else:
            r = p.add_run(smart_quotes(piece))
            set_run_font(r, size=size, color=color)


def add_tip(doc, lines):
    """提示框：浅蓝底 + 左侧竖条。多行用软换行保持在同一个框里。"""
    p = doc.add_paragraph()
    set_spacing(p, before=5, after=11, line=LINE_TIP)
    p.paragraph_format.left_indent = Cm(0.25)
    p.paragraph_format.right_indent = Cm(0.1)
    shade_paragraph(p, C_TIP_BG)
    para_borders(p, left=C_TIP_BAR, left_size=18)

    for i, ln in enumerate(lines):
        if i:
            p.add_run().add_break()
        emit_inline(p, ln, size=10, color=C_TIP_TEXT)

    # 提示框内的粗体用稍深的同色系，避免在浅底上过于跳
    for r in p.runs:
        if r.font.bold and r.font.color and r.font.color.rgb:
            r.font.color.rgb = RGBColor.from_string("0D3F73")
    return p


def add_rule(doc):
    """分隔线。

    ⚠️ 这里**只留白、不画线**：文章里 `---` 总是紧邻一个 `##` 标题，
    而 `##` 标题自己已经有下边框 —— 再画一条就成了双线，很碎。
    """
    p = doc.add_paragraph()
    set_spacing(p, before=0, after=0, line=LINE_SPACER)
    r = p.add_run()
    set_run_font(r, size=1)
    return p


def add_bullet(doc, text):
    p = doc.add_paragraph()
    set_spacing(p, after=4)
    p.paragraph_format.left_indent = Cm(0.75)
    p.paragraph_format.first_line_indent = Cm(-0.45)
    r = p.add_run("·  ")
    set_run_font(r, size=BODY_PT, bold=True, color=C_H2)
    emit_inline(p, text)
    return p


def add_numbered(doc, num, text):
    p = doc.add_paragraph()
    set_spacing(p, after=4)
    p.paragraph_format.left_indent = Cm(0.85)
    p.paragraph_format.first_line_indent = Cm(-0.55)
    r = p.add_run(f"{num}.  ")
    set_run_font(r, size=BODY_PT, bold=True, color=C_H2)
    emit_inline(p, text)
    return p


def add_table(doc, rows):
    """表格：深蓝表头 + 隔行浅底 + 细边框。"""
    cols = len(rows[0])
    t = doc.add_table(rows=len(rows), cols=cols)
    t.alignment = WD_TABLE_ALIGNMENT.CENTER
    t.autofit = True

    # 细边框
    tbl_pr = t._tbl.tblPr
    borders = OxmlElement("w:tblBorders")
    for edge in ("top", "left", "bottom", "right", "insideH", "insideV"):
        e = OxmlElement(f"w:{edge}")
        e.set(qn("w:val"), "single")
        e.set(qn("w:sz"), "4")
        e.set(qn("w:space"), "0")
        e.set(qn("w:color"), C_TABLE_LINE)
        borders.append(e)
    tbl_pr.append(borders)

    for ri, row in enumerate(rows):
        for ci, cell_text in enumerate(row):
            cell = t.cell(ri, ci)
            cell.text = ""
            p = cell.paragraphs[0]
            set_spacing(p, before=3, after=3, line=LINE_SMALL)

            # 单元格底纹要写在 tcPr 上（段落底纹在表格里不可靠）
            tcpr = cell._tc.get_or_add_tcPr()
            shd = OxmlElement("w:shd")
            shd.set(qn("w:val"), "clear")
            shd.set(qn("w:color"), "auto")
            fill = C_TABLE_HEAD if ri == 0 else (C_TABLE_ALT if ri % 2 == 0 else "FFFFFF")
            shd.set(qn("w:fill"), fill)
            tcpr.append(shd)

            clean = smart_quotes(cell_text.replace("**", ""))
            if ri == 0:
                r = p.add_run(clean)
                set_run_font(r, size=10, bold=True, color="FFFFFF")
            elif ci == 0:
                # 第一列当"名称列"加粗
                r = p.add_run(clean)
                set_run_font(r, size=10, bold=True, color=C_H1)
            else:
                emit_inline(p, cell_text, size=10)

    # 表后留白
    tail = doc.add_paragraph()
    set_spacing(tail, after=4, line=LINE_SPACER)
    r = tail.add_run()
    set_run_font(r, size=1)
    return t


# ============================================================
#  解析 markdown
# ============================================================

def build(md_text: str) -> Document:
    doc = Document()

    # 页面：A4 + 适中页边距
    sec = doc.sections[0]
    sec.page_width = Cm(21.0)
    sec.page_height = Cm(29.7)
    sec.top_margin = Cm(2.2)
    sec.bottom_margin = Cm(2.0)
    sec.left_margin = Cm(2.4)
    sec.right_margin = Cm(2.4)
    add_page_number_footer(sec)

    # 默认字体（兜底）
    normal = doc.styles["Normal"]
    normal.font.name = FONT
    normal.font.size = Pt(BODY_PT)
    normal.element.rPr.rFonts.set(qn("w:eastAsia"), FONT)

    lines = md_text.splitlines()
    i = 0
    seen_h1 = False
    ol_counter = 0

    while i < len(lines):
        raw = lines[i]
        line = raw.rstrip()
        stripped = line.strip()

        # 空行
        if not stripped:
            ol_counter = 0
            i += 1
            continue

        # 标题
        if stripped.startswith("# ") and not stripped.startswith("## "):
            add_title(doc, stripped[2:].strip())
            i += 1
            continue

        if stripped.startswith("## ") and not stripped.startswith("### "):
            title = stripped[3:].strip()
            if not seen_h1:
                seen_h1 = True
            add_h1(doc, title)
            i += 1
            continue

        if stripped.startswith("### "):
            add_h2(doc, stripped[4:].strip())
            i += 1
            continue

        # 分隔线
        if stripped in ("---", "***", "___"):
            add_rule(doc)
            i += 1
            continue

        # 引用块（可能连续多行）
        if stripped.startswith(">"):
            block = []
            while i < len(lines) and lines[i].strip().startswith(">"):
                t = lines[i].strip()[1:].strip()
                if t:
                    block.append(t)
                i += 1
            # 第一个标题之前的引用 = 副标题
            if not seen_h1 and len(block) >= 1:
                add_subtitle(doc, " ".join(block))
            else:
                add_tip(doc, block)
            continue

        # 表格
        if stripped.startswith("|"):
            rows = []
            while i < len(lines) and lines[i].strip().startswith("|"):
                cells = [c.strip() for c in lines[i].strip().strip("|").split("|")]
                # 跳过 markdown 的对齐分隔行 |---|---|
                if not all(re.fullmatch(r":?-{2,}:?", c or "-") for c in cells):
                    rows.append(cells)
                i += 1
            if rows:
                add_table(doc, rows)
            continue

        # 有序列表
        m = re.match(r"^(\d+)\.\s+(.*)$", stripped)
        if m:
            ol_counter += 1
            add_numbered(doc, m.group(1), m.group(2))
            i += 1
            continue

        # 无序列表
        if stripped.startswith("- "):
            add_bullet(doc, stripped[2:].strip())
            i += 1
            continue

        # 普通段落
        #
        # 整行都是粗体（`**问题？**`）时把段后距压小 —— 它其实是下一段的小标题，
        # 问答区就靠这个让"问"和"答"贴在一起，而不是散成两段。
        # 问答区的"问句"：整行粗体且以 ？ 结尾 —— 让它与下面的答案贴紧。
        # 其余整行粗体（如"屏幕一亮，充电就慢得离谱。"）是强调句，保留正常段距。
        is_question = (
            stripped.startswith("**")
            and stripped.endswith("**")
            and stripped.count("**") == 2
            and stripped[-3] in "？?"
        )
        add_body(doc, stripped, after=2 if is_question else 8)
        i += 1

    return doc


def main():
    if not SRC.exists():
        print(f"✗ 找不到源文件：{SRC}")
        return 1

    md = SRC.read_text(encoding="utf-8")

    # 引号成对性自检 —— 弯引号转换是按"一开一合"交替的，
    # 若原文引号数不是偶数，转换结果会错位，必须先发现
    n_dq = md.count('"')
    if n_dq % 2 != 0:
        print(f"⚠ 直双引号有 {n_dq} 个（奇数），弯引号转换会错位，请先检查原文")

    doc = build(md)

    doc.core_properties.title = "澎湃充控 HyperPower —— 亮屏也能快充，温控交给你自己"
    doc.core_properties.author = "kino516"
    doc.core_properties.comments = "KernelSU / APatch 温控充电模块 · GPL-3.0 开源"

    OUT.parent.mkdir(parents=True, exist_ok=True)
    doc.save(OUT)

    size_kb = OUT.stat().st_size / 1024
    print(f"✓ 已生成：{OUT}")
    print(f"  {size_kb:.1f} KB")
    print(f"  段落数: {len(doc.paragraphs)}  表格数: {len(doc.tables)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
