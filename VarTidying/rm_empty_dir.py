import os
import argparse

import stat

def set_readwrite(path):
    """移除文件或文件夹的只读属性"""
    try:
        # 检查路径是否存在
        if not os.path.exists(path):
            print(f"路径不存在: {path}")
            return False
        
        # 获取当前文件属性
        current_attrs = os.stat(path).st_mode
        
        # 检查是否只读 (判断是否有写权限)
        if not current_attrs & stat.S_IWRITE:
            # 移除只读属性 - 添加用户写权限
            os.chmod(path, current_attrs | stat.S_IWRITE)
            print(f"已移除只读属性: {path}")
            return True
        else:
            print(f"无需修改 (非只读): {path}")
            return False
            
    except Exception as e:
        print(f"修改属性出错 [{path}]: {str(e)}")
        return False

def is_empty_directory(path):
    """检查目录是否为空（包括所有文件和子目录）"""
    return len(os.listdir(path)) == 0

def rm_empty_dirs(target_dir, dry_run):
    processed_dirs = []

    # 收集所有需要处理的非隐藏目录
    for root, dirs, _ in os.walk(target_dir):
        # 过滤掉以.开头的隐藏目录（原地修改dirs列表）
        dirs[:] = [d for d in dirs if not d.startswith('.')]
        processed_dirs.append(root)

    # 按目录深度降序排序（从最深开始处理）
    processed_dirs.sort(key=lambda p: p.count(os.sep), reverse=True)

    for dir_path in processed_dirs:
        if is_empty_directory(dir_path):
            if dir_path == target_dir and not args.dry_run:
                print(f'警告: 正在删除根目录 {dir_path}（可能需要管理员权限）')
            
            if dry_run:
                print(f'[试运行] 发现空目录: {dir_path}')
            else:
                try:
                    os.rmdir(dir_path)
                    print(f'已删除空目录: {dir_path}')
                except OSError as e:
                    print(f'删除失败: {dir_path} ({e.strerror}) 尝试removedirs')
                    try:
                        os.removedirs(dir_path)
                    except OSError as e:
                        print(f'removedirs 失败: {dir_path} ({e.strerror})--- 尝试修改属性')
                        if(set_readwrite(dir_path)):
                            try:
                                os.removedirs(dir_path)
                                print(f'已删除空目录: {dir_path}')
                            except OSError as e:
                                print(f'再次删除失败: {dir_path} ({e.strerror})')


def main():
    parser = argparse.ArgumentParser(description='删除指定目录下的空目录')
    parser.add_argument('directory', help='要清理的目标目录')
    parser.add_argument('--dry-run', action='store_true',
                        help='试运行模式，只显示将要执行的操作而不实际删除')
    args = parser.parse_args()

    target_dir = os.path.abspath(args.directory)
    rm_empty_dirs(target_dir, args.dry_run)
    
if __name__ == '__main__':
    main()