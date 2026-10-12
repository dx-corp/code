"""Regression tests for the bounded palette convention guard."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'check_ui_consistency', Path(__file__).with_name('check-ui-consistency.py')
)
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class ConsistencyTests(unittest.TestCase):
    def test_rejects_rgb_indexed_and_named_colors_with_line_numbers(self):
        self.assertEqual(check.violations(
            'let x = Color::Rgb(1, 2, 3);\nColor :: Indexed(2);\nColor::Red;'
        ), [(1, 'Color::Rgb'), (2, 'Color :: Indexed'), (3, 'Color::Red')])

    def test_ignores_comments_strings_and_semantic_theme(self):
        self.assertEqual(check.violations('''
            // Color::Red
            /* nested /* Color::Blue */ Color::White */
            let prose = "Color::Cyan";
            let raw = r##"Color::Rgb(1, 2, 3)"##;
            let color = current_ui_theme().text;
        '''), [])

    def test_test_exemption_does_not_hide_following_production(self):
        self.assertEqual(check.violations('''#[cfg(test)]
mod tests { fn fixture() { let color = Color::Red; } }
fn render() { let color = Color::Blue; }
'''), [(3, 'Color::Blue')])

    def test_inline_test_method_does_not_hide_later_methods(self):
        self.assertEqual(check.violations('''impl Picker {
#[cfg(test)] fn fixture() { Color::Red; }
fn render() { Color::White; }
}'''), [(3, 'Color::White')])

    def test_scope_excludes_palette_owner_and_unmigrated_components(self):
        self.assertEqual(len(check.FILES), 10)
        self.assertNotIn('packages/tui-kit-rs/src/themes.rs', check.FILES)
        self.assertNotIn('packages/tui-rs/src/app.rs', check.FILES)
        self.assertTrue(all((check.ROOT / p).is_file() for p in check.FILES))


if __name__ == '__main__':
    unittest.main()
