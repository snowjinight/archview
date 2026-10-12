import os
import shutil
import argparse
from pathlib import Path
import stat
from rm_empty_dir import rm_empty_dirs

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

# 新增：删除空目录
def delete_empty_parents(start_dir, stop_dir):
    current_dir = Path(start_dir).parent
    while current_dir != Path(stop_dir).parent:
        try:
            if not any(current_dir.iterdir()):
                current_dir.rmdir()
                print(f"已删除空目录：{current_dir}")
                current_dir = current_dir.parent
            else:
                break
        except Exception as e:
            print(f"删除目录失败：{current_dir} - {str(e)} --- 尝试修改读写属性")
            set_readwrite(current_dir)
            break

def organize_invalid_files(input_txt, base_dir, keep_structure=False):
    # 定义目标目录
    dest_dirs = {
        "[版本号错误]": "错误_version",
        "[格式错误]": "错误_format",
        "[非VAR文件]": "错误_novar",
        "[作者名缺失]": "错误_author",
        "[内容缺失]": "错误_content",
    }
    
    # 创建目标目录
    for folder in dest_dirs.values():
        (Path(base_dir) / folder).mkdir(parents=True, exist_ok=True)

    # 读取并处理文件
    with open(input_txt, 'r', encoding='utf-8') as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            
            # 解析行内容
            if '] ' in line:
                prefix, filepath = line.split('] ', 1)
                prefix += ']'  # 补回被分割的右括号
                
                # 只处理指定错误类型
                if prefix in dest_dirs:
                    src = Path(filepath)
                    dest_dir = Path(base_dir) / dest_dirs[prefix]
                    
                    if src.exists():
                        try:
                            # 根据参数选择移动方式
                            if keep_structure:
                                try:
                                    # 转换为绝对路径确保路径一致性
                                    base_abs = Path(base_dir).resolve()
                                    src_abs = src.resolve()
                                    # 获取安全相对路径
                                    relative_path = src_abs.relative_to(base_abs)
                                    target = dest_dir / relative_path
                                except ValueError:
                                    # 当文件直接位于根目录时回退到文件名
                                    target = dest_dir / src.name
                            else:
                                target = dest_dir / src.name
                            
                            # 确保目标目录存在
                            target.parent.mkdir(parents=True, exist_ok=True)
                            
                            # 处理重复文件名
                            if target.exists():
                                print(f"发现重复文件：{src.name}")
                                # 自动重命名：添加计数器后缀
                                counter = 1
                                while target.exists():
                                    new_name = f"{src.stem}_{counter}{src.suffix}"
                                    target = dest_dir / new_name
                                    counter += 1
                                print(f"自动重命名为：{target.name}")
                                
                            shutil.move(str(src), str(target))
                            # delete_empty_parents(src, base_dir)
                            # print(f"移动成功：{src.name} -> {dest_dirs[prefix]}/")
                        except Exception as e:
                            print(f"移动失败：{src.name} - 错误：{str(e)}")
                    else:
                        print(f"文件不存在：{src.name}")

if __name__ == "__main__":
    # 配置命令行参数解析
    parser = argparse.ArgumentParser(
        description="自动整理无效文件",
        formatter_class=argparse.RawTextHelpFormatter
    )
    parser.add_argument(
        "-d", "--dir",
        required=True,
        help="指定包含invalid_files.txt的目录（必须）\n例: -d E:/game/vam_add"
    )
    parser.add_argument(
        "-f", "--filename",
        default="invalid_files.txt",
        help="指定错误列表文件名（可选）\n默认: invalid_files.txt\n例: -f my_errors.txt"
    )
    parser.add_argument(
        "-k", "--keep-structure",
        action="store_true",
        help="保持原始目录结构（可选）\n启用后文件将按原始路径结构保存\n例: -k"
    )
    parser.add_argument(
        "-r", "--rm-empty",
        action="store_true",
        help="删除空目录\n例: -r"
    )
    
    # 解析参数
    args = parser.parse_args()
    
    # 构建完整文件路径
    target_dir = Path(args.dir)
    input_file = target_dir / args.filename
    
    # 验证文件存在
    if not input_file.exists():
        print(f"错误：目录 {target_dir} 下未找到 {args.filename}")
        exit(1)
        
    organize_invalid_files(input_file, target_dir, args.keep_structure)

    if(args.rm_empty):
        rm_empty_dirs(target_dir, 0)
