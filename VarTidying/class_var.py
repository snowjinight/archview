import os
import shutil
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


def organize_var_files(root_dir):
    root_dir = os.path.abspath(root_dir)
    processed_authors = set()
    temp_workspace = os.path.join(root_dir, "__TEMP_WORKSPACE__")
    
    # 创建临时工作区
    os.makedirs(temp_workspace, exist_ok=True)

    try:
        with open(os.path.join(root_dir, 'invalid_files.txt'), 'a', encoding='utf-8') as log_file:
            log_file.write(f"\n{'='*30} 新的整理会话 {'='*30}\n")

            # 修改遍历逻辑：排除临时工作区
            for foldername, subdirs, filenames in os.walk(root_dir):
                # 跳过临时工作区及其子目录
                if foldername.startswith(temp_workspace):
                    subdirs[:] = []
                    continue

                for filename in filenames:
                    # 跳过日志文件本身
                    current_file = os.path.join(foldername, filename)
                    if os.path.abspath(current_file) == os.path.join(root_dir, 'invalid_files.txt'):
                        continue
                    
                    # 记录非var文件
                    if not filename.endswith('.var'):
                        log_file.write(f"[非VAR文件] {os.path.join(foldername, filename)}\n")
                        continue

                    # 解析文件名并记录无效格式
                    parts = filename.split('.')
                    # 格式校验（至少包含作者.内容.版本.var四部分）
                    if len(parts) < 4 or parts[-1] != 'var':
                        log_file.write(f"[格式错误] {os.path.join(foldername, filename)}\n")
                        continue

                    # 提取并验证版本号
                    version = parts[-2]
                    if not version.isdigit():
                        log_file.write(f"[版本号错误] {os.path.join(foldername, filename)}\n")
                        continue

                    # 合并可能包含点的内容部分
                    author = parts[0].strip()  # 去除首尾空格
                    content = '.'.join(parts[1:-2]).strip()  # 去除内容部分首尾空格

                    # 验证作者和内容部分非空
                    if not author:
                        log_file.write(f"[作者名缺失] {os.path.join(foldername, filename)}\n")
                        continue
                    if not content:
                        log_file.write(f"[内容缺失] {os.path.join(foldername, filename)}\n")
                        continue

                    # 修改作者目录路径到临时工作区
                    author_dir = os.path.join(temp_workspace, author)  # 关键修改点
                    os.makedirs(author_dir, exist_ok=True)
                    processed_authors.add(author)

                    src_path = os.path.join(foldername, filename)
                    dest_path = os.path.join(author_dir, filename)

                    try:
                        if(os.path.samefile(dest_path, src_path)):
                            continue
                    except OSError:
                        print(f"{dest_path} not exists")

                    # 处理重复文件
                    if os.path.exists(dest_path):
                        try:
                            os.remove(src_path)
                            print(f"已删除重复文件: {src_path}")
                        except PermissionError:
                            print("删除失败，尝试修改权限...")
                            if set_readwrite(src_path):
                                try:
                                    os.remove(src_path)
                                    print(f"已删除重复文件: {src_path}")
                                except Exception as e:
                                    log_file.write(f"[删除失败] {src_path} - {str(e)}\n")
                            else:
                                log_file.write(f"[权限修改失败] {src_path}\n")
                    else:
                        shutil.move(src_path, dest_path)
                        print(f"已移动文件到: {dest_path}")

        # 合并临时工作区到根目录
        for author_dir in os.listdir(temp_workspace):
            src = os.path.join(temp_workspace, author_dir)
            dest = os.path.join(root_dir, author_dir)

            # 合并目录内容
            print(f"开始合并目录内容: {src}到: {dest}")
            if os.path.exists(dest):
                # 处理已存在目录的合并
                for item in os.listdir(src):
                    item_src = os.path.join(src, item)
                    item_dest = os.path.join(dest, item)
                    if os.path.exists(item_dest):
                        os.remove(item_src)  # 删除重复文件
                        print(f"目录已存在，文件已存在,删除--->{item_src}")
                    else:
                        shutil.move(item_src, dest)
                        print(f"目录已存在，文件不存在，移动-->{item_src}到: {dest}")
                os.rmdir(src)
            else:
                shutil.move(src, root_dir)
                print(f"目录不存在，移动-->{src}到: {root_dir}")

    finally:
        # 安全清理临时工作区（只删除空目录）
        if os.path.exists(temp_workspace):
            # 自底向上遍历目录树
            for root, dirs, files in os.walk(temp_workspace, topdown=False):
                # 删除空子目录
                for name in dirs:
                    dir_path = os.path.join(root, name)
                    try:
                        os.rmdir(dir_path)  # 只有空目录会被删除
                        print(f"已清理空临时目录: {dir_path}")
                    except OSError:
                        pass  # 忽略非空目录
            # 最后尝试删除临时工作区本身（如果为空）
            try:
                os.rmdir(temp_workspace)
            except OSError:
                pass

    # 新增空目录清理逻辑
    for foldername, subdirs, filenames in os.walk(root_dir, topdown=False):
        if foldername == root_dir:  # 跳过根目录
            continue
        if not os.listdir(foldername):  # 检查目录是否为空
            os.rmdir(foldername)
            print(f"已删除空目录: {foldername}")

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='整理VAR文件到作者目录')
    parser.add_argument('root_dir', help='要处理的根目录路径')
    args = parser.parse_args()
    
    organize_var_files(args.root_dir)
