#!/usr/bin/env python3
"""
路径映射工具库
用于在容器环境中验证和转换路径映射
支持 Docker volume mappings 的路径转换和验证
"""
import os
import sys
import yaml
from pathlib import Path
from typing import List, Dict, Tuple, Optional


class PathMappingError(Exception):
    """路径映射相关的异常"""
    pass


class PathMapper:
    """路径映射管理器"""
    
    def __init__(self, config_file: str = "build_config_ext.yml"):
        """
        初始化路径映射器
        
        Args:
            config_file: 配置文件路径
        """
        self.config_file = config_file
        self.volume_mappings = []
        self.load_config()
    
    def load_config(self):
        """加载配置文件中的路径映射"""
        try:
            config_path = Path(self.config_file)
            if not config_path.exists():
                raise PathMappingError(f"配置文件不存在: {self.config_file}")
            
            with open(config_path, 'r', encoding='utf-8') as f:
                config = yaml.safe_load(f)
            
            volume_mappings = config.get('volume_mappings', [])
            if not volume_mappings:
                raise PathMappingError("配置文件中未找到 volume_mappings 配置")
            
            # 解析路径映射
            for mapping in volume_mappings:
                if ':' not in mapping:
                    raise PathMappingError(f"路径映射格式错误: {mapping}")
                
                container_path, host_path = mapping.split(':', 1)
                container_path = container_path.strip()
                host_path = host_path.strip()
                
                if not container_path or not host_path:
                    raise PathMappingError(f"路径映射包含空路径: {mapping}")
                
                self.volume_mappings.append((container_path, host_path))
            
            print(f"已加载 {len(self.volume_mappings)} 个路径映射:")
            for container_path, host_path in self.volume_mappings:
                print(f"  {container_path} -> {host_path}")
                
        except yaml.YAMLError as e:
            raise PathMappingError(f"配置文件格式错误: {e}")
        except Exception as e:
            raise PathMappingError(f"加载配置失败: {e}")
    
    def is_path_allowed(self, container_path: str) -> bool:
        """
        检查容器路径是否在允许的映射列表中
        
        Args:
            container_path: 容器内的路径
            
        Returns:
            bool: 是否允许访问该路径
        """
        container_path = Path(container_path).resolve()
        
        for mapped_container_path, _ in self.volume_mappings:
            mapped_container_path = Path(mapped_container_path).resolve()
            
            # 检查是否是子目录或相同目录
            try:
                container_path.relative_to(mapped_container_path)
                return True
            except ValueError:
                # 不是子目录，继续检查下一个
                continue
        
        return False
    
    def convert_to_host_path(self, container_path: str) -> str:
        """
        将容器路径转换为宿主机路径
        
        Args:
            container_path: 容器内的路径
            
        Returns:
            str: 宿主机路径
            
        Raises:
            PathMappingError: 如果路径不在允许的映射列表中
        """
        container_path = Path(container_path).resolve()
        
        for mapped_container_path, mapped_host_path in self.volume_mappings:
            mapped_container_path = Path(mapped_container_path).resolve()
            
            # 检查是否是子目录或相同目录
            try:
                relative_path = container_path.relative_to(mapped_container_path)
                host_path = Path(mapped_host_path) / relative_path
                return str(host_path)
            except ValueError:
                # 不是子目录，继续检查下一个
                continue
        
        raise PathMappingError(
            f"路径 {container_path} 不在允许的映射列表中。"
            f"允许的映射: {[mapping[0] for mapping in self.volume_mappings]}"
        )
    
    def convert_to_container_path(self, host_path: str) -> str:
        """
        将宿主机路径转换为容器路径
        
        Args:
            host_path: 宿主机路径
            
        Returns:
            str: 容器路径
            
        Raises:
            PathMappingError: 如果路径不在允许的映射列表中
        """
        host_path = Path(host_path).resolve()
        
        for mapped_container_path, mapped_host_path in self.volume_mappings:
            mapped_host_path = Path(mapped_host_path).resolve()
            
            # 检查是否是子目录或相同目录
            try:
                relative_path = host_path.relative_to(mapped_host_path)
                container_path = Path(mapped_container_path) / relative_path
                return str(container_path)
            except ValueError:
                # 不是子目录，继续检查下一个
                continue
        
        raise PathMappingError(
            f"宿主机路径 {host_path} 不在允许的映射列表中。"
            f"允许的映射: {[mapping[1] for mapping in self.volume_mappings]}"
        )
    
    def validate_and_convert(self, container_path: str) -> str:
        """
        验证并转换路径（容器路径 -> 宿主机路径）
        
        Args:
            container_path: 容器内的路径
            
        Returns:
            str: 宿主机路径
            
        Raises:
            PathMappingError: 如果路径验证失败
        """
        if not self.is_path_allowed(container_path):
            raise PathMappingError(
                f"路径访问被拒绝: {container_path}。"
                f"该路径不在允许的映射列表中。"
            )
        
        return self.convert_to_host_path(container_path)
    
    def get_docker_volume_args(self) -> List[str]:
        """
        获取 Docker volume 参数列表
        
        Returns:
            List[str]: Docker -v 参数列表
        """
        volume_args = []
        for container_path, host_path in self.volume_mappings:
            volume_args.extend(['-v', f'{host_path}:{container_path}'])
        return volume_args


def main():
    """测试函数"""
    try:
        # 创建路径映射器
        mapper = PathMapper()
        
        print("\n=== 路径映射测试 ===")
        
        # 测试路径转换
        test_paths = [
            "/opt/pa/prjs/nokube-rs/src/main.rs",
            "/opt/pa/prjs/nokube-rs/Cargo.toml", 
            "/tmp/test.txt",
            "/var/run/docker.sock",
            "/unauthorized/path",
        ]
        
        for test_path in test_paths:
            print(f"\n测试路径: {test_path}")
            try:
                if mapper.is_path_allowed(test_path):
                    host_path = mapper.convert_to_host_path(test_path)
                    print(f"  ✅ 允许访问")
                    print(f"  🔄 容器路径: {test_path}")
                    print(f"  🏠 宿主机路径: {host_path}")
                else:
                    print(f"  ❌ 拒绝访问")
            except PathMappingError as e:
                print(f"  ❌ 错误: {e}")
        
        print("\n=== Docker Volume 参数 ===")
        volume_args = mapper.get_docker_volume_args()
        print("Docker 运行命令:")
        print(f"docker run {' '.join(volume_args)} <image> <command>")
        
    except Exception as e:
        print(f"错误: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()