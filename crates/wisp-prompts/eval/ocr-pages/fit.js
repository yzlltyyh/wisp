// 手工断行的段落（带 .fit 的容器，每行一个 .l 块）：按最宽一行的自然宽度定栏宽，再两端对齐。
// 栏宽取决于本机字体，断行位置只取决于页面源码，所以换机器渲染时行尾的断词连字符、被折开的网址都还在原来的位置。
// 渲染脚本读取 <html data-ready> 判断排版已经完成，读取 data-spread（段中最窄一行与最宽一行之比）提示哪一页的行长差得太多。
function lineWidth(line) {
  const range = document.createRange()
  range.selectNodeContents(line)
  return range.getBoundingClientRect().width
}

function fitColumns() {
  let spread = 1
  for (const col of document.querySelectorAll('.fit')) {
    col.style.width = ''
    col.classList.add('measuring')
    const lines = [...col.querySelectorAll('.l')]
    const widest = Math.max(...lines.map(lineWidth))
    for (const line of lines) {
      // 段尾一行本来就短，不参与比较
      if (line.nextElementSibling) spread = Math.min(spread, lineWidth(line) / widest)
    }
    col.classList.remove('measuring')
    const cs = getComputedStyle(col)
    const chrome = ['paddingLeft', 'paddingRight', 'borderLeftWidth', 'borderRightWidth']
      .map((k) => parseFloat(cs[k]))
      .reduce((a, b) => a + b, 0)
    // 多留 1 px：自然宽度带小数，向上取整后最宽的一行也不会被挤到下一行
    col.style.width = `${Math.ceil(widest + chrome) + 1}px`
  }
  document.documentElement.dataset.spread = spread.toFixed(3)
  document.documentElement.dataset.ready = '1'
}

document.fonts.ready.then(fitColumns)
