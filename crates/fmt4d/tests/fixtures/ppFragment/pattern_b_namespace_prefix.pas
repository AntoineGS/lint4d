unit T;

interface

implementation

procedure SetDecimalSeparator;
begin
  {$ifdef DELPHI_XE2_UP}FormatSettings.{$endif}DecimalSeparator := '.';
  {$ifdef DELPHI_XE2_UP}FormatSettings.{$endif}DateSeparator := '/';
end;

end.
